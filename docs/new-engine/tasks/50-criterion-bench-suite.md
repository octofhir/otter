# Task 50 — Criterion bench suite

## Goal

Create the first real Criterion bench targets so future perf
work has a baseline.

## Scope

- `crates-next/otter-vm/benches/` already has `dispatch.rs` (10 000
  NOPs). Add:
  - `int_loop.rs` — sum 1..1_000_000 with smi arithmetic.
  - `string_concat.rs` — `s += piece` 1 000-iteration loop.
  - `property_load.rs` — repeated `obj.x` reads on a small object
    (depends on task 17 / 18).
  - `call_overhead.rs` — empty function call in a tight loop
    (depends on the foundation `Op::Call`).
- Each bench prints throughput in iterations per second; commit
  the human-readable summary into the task closure note.

## Out of scope

- Benchmark CI gates — that is a follow-up once the suite is
  stable.
- Cross-engine comparisons (Node / Bun / Deno).

## Files / directories you may touch

- `crates-next/otter-vm/benches/`
- `crates-next/otter-runtime/benches/` (if a runtime-level harness
  is useful for `call_overhead`).

## Acceptance criteria

- `cargo bench -p otter-vm --no-run` succeeds.
- Each new bench file documents its baseline number on a developer
  machine in its module docstring.

## Verification commands

```bash
cargo bench -p otter-vm --no-run
```

## Risks

- Bench targets that depend on tasks 17 / 21 must wait for those
  to land or use string-only stubs.

## Status

- not started
