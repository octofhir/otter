# Task 73 — OOM, cap enforcement, `Runtime::max_heap_bytes` load-bearing

## Status

- [ ] `OutOfMemory` error type
- [ ] `tracked_bytes` accounting on alloc + `reserve_bytes` / `release_bytes`
- [ ] cap **refuses** the alloc (does not just set a flag)
- [ ] `Runtime::max_heap_bytes` plumbed end-to-end
- [ ] `OtterError::OutOfMemory` surfaces from a script-driven OOM
- [ ] gates green

## Goal

Make `Runtime::max_heap_bytes` load-bearing instead of informational.
A script that allocates past the cap must surface
`OtterError::OutOfMemory` (catchable as `RangeError` from JS, after
task 84's spec-shaped wrapper). The legacy bug — *cap check sets the
OOM flag but the alloc still runs* — must not be reproduced.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §1.1
F4, §2.1 ("Caveat" on legacy `would_exceed_limit`), §2.3
inheritance ledger row 4, §7.5.

## Scope

1. **`OutOfMemory` error type** in `crates-next/otter-gc/src/oom.rs`,
   `thiserror`-derived.
2. **Heap cap config** — `GcHeap::with_max_heap_bytes(cap: u64)`;
   `0` = disabled.
3. **`alloc` change** — `pub fn alloc<T>(&mut self, value: T) ->
   Result<Gc<T>, OutOfMemory>`. Cap is checked **before** the slot is
   allocated. On overflow: emergency `collect_full(roots)`, retry
   once; if still over → `Err(OutOfMemory)`.
4. **`reserve_bytes` / `release_bytes`** — for off-slot accounting
   (`Vec` capacity inside payloads). Same retry-once-then-fail
   pattern.
5. **`oom_flag: Arc<AtomicBool>`** — kept for cooperative-cancellation
   parity with the existing watchdog, but never the *primary* signal:
   alloc returns `Err` directly.
6. **Runtime plumbing** — in `crates-next/otter-runtime/src/lib.rs`:
   - Pass `config.max_heap_bytes` to `GcHeap::with_max_heap_bytes`
     when constructing the runtime.
   - Update the `max_heap_bytes` getter docstring: drop "currently
     informational; the foundation slice does not yet enforce heap
     caps".
   - Map `otter_gc::OutOfMemory` → `OtterError::OutOfMemory` in
     `error.rs`.

## Tests

- `oom_alloc_returns_err.rs` (gc unit test): cap = 1 KiB; alloc
  larger than 1 KiB; assert `Err(OutOfMemory)`.
- `oom_emergency_collect_recovers.rs`: cap = 8 KiB; allocate 6 KiB,
  drop the only handle, alloc another 6 KiB; assert success (the
  emergency collect freed the first one).
- `oom_zero_disables.rs`: cap = 0; allocate 100 MiB; success.
- `runtime_oom_surfaces_as_error.rs` (runtime integration test):
  `Runtime::builder().max_heap_bytes(2 * 1024 * 1024).build()` then
  run a script that allocates an unbounded array; assert
  `Err(OtterError::OutOfMemory { .. })`.
- **No** new test262 sweep — that comes in task 84.

## Out of scope

- `RangeError`-shaped JS-visible error wrapping (task 84 wires this
  through a `try { … } catch (e) { e instanceof RangeError }` shape).
- Per-type retained-byte breakdown (task 74).

## Validation gates

- [ ] Existing `crates-next/otter-runtime` tests still pass.
- [ ] All four tests above pass.
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `Runtime::max_heap_bytes` docstring no longer claims it is
  informational.

## Closing

Gates from [`README.md`](./README.md#closing-a-task), tick 73 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md), delete this
file.
