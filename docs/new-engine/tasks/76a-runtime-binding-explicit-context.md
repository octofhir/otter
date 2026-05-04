# Task 76A — Runtime binding cleanup: explicit context, no thread-local heap

## Status

- [x] `RuntimeCx` / `NativeCtx` introduced for VM and native entrypoints
      (`crates-next/otter-vm/src/runtime_cx.rs`).
- [x] Product code no longer calls `GcHeap::with_thread_default*` —
      audit `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm/src crates-next/otter-runtime/src`
      returns only doc-comment hits in `runtime_cx.rs`.
- [~] `GcHeap` thread-default helpers retained as `#[doc(hidden)]`
      transitional shims (heap.rs §`enter_thread_default` etc.). Each
      method carries a clear migration note pointing at this task.
      They will be deleted once tasks 77-83 finish caller migration.
- [x] `Gc<T>`, `Local<'gc, T>`, `GcHeap`, `HandleScope`, `Interpreter`,
      `NativeCtx<'_>` proven `!Send + !Sync` via `static_assertions::assert_not_impl_any!`
      in `crates-next/otter-gc/src/lib.rs` and `crates-next/otter-vm/src/lib.rs`.
- [x] compile-fail tests reject `Gc<T>` / `Local<'gc, T>` / `GcHeap`
      captured into a `Send` bound (the shape `tokio::spawn` requires) —
      `crates-next/otter-vm/tests/compile_fail/`.
- [x] gates green: `cargo build --workspace`, `cargo test -p otter-gc -p otter-vm`,
      `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
      `cargo fmt --all -- --check`.

## Goal

Make the single-mutator invariant visible in Rust types before the rest of
the GC migration proceeds. Tasks 77-83 must not build on thread-local heap
lookup. Every read/write/barrier path should know which runtime context owns
the object.

This task is a blocker for task 77. If task 77 has already started locally,
fold this cleanup into that branch before closing 77.

## Source

- [`../adr/0005-async-runtime-binding.md`](../adr/0005-async-runtime-binding.md)
- [`../gc-architecture.md`](../gc-architecture.md) §6.2, §6.3
- [`70-gc-master-tracker.md`](./70-gc-master-tracker.md)

## Scope

1. **Context types.**
   Add explicit internal context types in `crates-next/otter-vm` /
   `crates-next/otter-runtime`:
   ```rust
   pub(crate) struct RuntimeCx<'rt> {
       pub(crate) state: &'rt mut RuntimeState,
       pub(crate) heap: &'rt mut GcHeap,
       // intrinsics, symbols, module state, diagnostics as needed
   }

   pub struct NativeCtx<'rt> {
       // public-to-native binding view; no direct Send/Sync
   }
   ```
   Exact field layout may differ, but the context must carry the heap
   base needed by allocation and write-barrier sites.

2. **API rule.**
   All object/array/map/promise/native APIs touched by tasks 77-83 use
   explicit context:
   ```rust
   obj.get(&mut cx, key)
   obj.set(&mut cx, key, value)
   cx.heap.write_barrier(owner, value)
   ```
   Do not introduce method-style APIs backed by thread-local lookup.

3. **Thread-local cleanup.**
   Remove `GcHeap::enter_thread_default`, `install_thread_default`,
   `with_thread_default`, and `with_thread_default_mut` from the product
   surface. If a test-only helper remains, it must be `pub(crate)` or
   `#[doc(hidden)]`, live under a clearly named testing module, and be
   forbidden in `crates-next/otter-vm` / `crates-next/otter-runtime`.

4. **Type-level `!Send + !Sync`.**
   Add static assertions for:
   - `GcHeap`
   - `Gc<T>`
   - `Local<'gc, T>`
   - `RuntimeCore` / `Interpreter`
   - `RuntimeCx<'_>` / `NativeCtx<'_>`

5. **Compile-fail tests.**
   Add `trybuild` or equivalent compile-fail fixtures proving these do
   not compile:
   - capturing `Gc<T>` in `tokio::spawn`;
   - capturing `Local<'gc, T>` in `tokio::spawn`;
   - capturing `NativeCtx<'_>` or `&mut RuntimeState` across `.await`;
   - returning internal `Value` from a `Send + 'static` host future.

6. **Runtime debug assertions.**
   Add debug-only isolate-id / generation checks where handles are
   decompressed or dereferenced. These are diagnostic tripwires, not a
   substitute for the type rules.

## Out of scope

- Public `Otter` / `RuntimeHandle` async facade (task 85).
- Worker API and structured clone (task 92).
- Large semantic migrations for object/array/map bodies beyond the minimum
  needed to remove thread-local heap access.

## Validation gates

- [ ] `rg "with_thread_default|enter_thread_default|install_thread_default" crates-next/otter-vm crates-next/otter-runtime` returns no product-code hits.
- [ ] `cargo test -p otter-gc -p otter-vm` green.
- [ ] Compile-fail suite green and includes a `tokio::spawn` capture case.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.

## Closing

Tick 76A in [70-gc-master-tracker.md](./70-gc-master-tracker.md). Leave
this file in place until tasks 77-83 close, because later agents need the
explicit-context rule visible.
