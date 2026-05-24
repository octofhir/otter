# Startup Snapshot Pipeline — Defer Decision

Status: **Deferred** until after Phases 4 and 5 (`otter_*` macros
shipped and inspector / trace surface stable). Scheduled review:
when 4.1 + 4.2 land or 2026-08, whichever first.

This is the deliverable for Task 3.3 of
[`architecture-refactor-plan-2026-05.md`](architecture-refactor-plan-2026-05.md).
It is *not* a design for the snapshot system. It is the written
record of the prerequisites that must hold before any design work
starts.

## Scope

"Snapshot" in this document means the **startup snapshot pipeline**
— V8 `mksnapshot` analog. Bootstrap takes a fresh `Interpreter`,
walks `BOOTSTRAP_ENTRIES`, installs every `BuiltinIntrinsic` plus
the per-realm intrinsic slots, and produces a `globalThis` graph
the embedder hands to user scripts. A startup snapshot serialises
that graph once, ships it as a binary blob, and at runtime
`Interpreter::new()` mmaps + relocates it instead of re-running
~1130 GC allocations.

Out of scope for this decision:

- [`crate::otter_gc::snapshot::HeapSnapshot`] — already shipped,
  in-memory walker for Rust-side retained-size asserts.
- [`crate::otter_gc::devtools_snapshot::write_heap_snapshot`] —
  already shipped, Chrome DevTools `.heapsnapshot` JSON exporter
  for production debugging.

Both stay; both are read-only diagnostics surfaces, not part of the
startup path.

## Decision

**Defer.** No snapshot design work — RFC, prototype, schema, or
otherwise — until every prerequisite below is satisfied. A future
RFC-4 will pick up the design from this list as input.

## Why defer

Three concrete risks if we start now:

1. **Bootstrap shape churn.** Phases 1 / 2 / 3 just rewrote the
   `Value`, frame layout, IC topology, and intrinsic registry.
   The snapshot is a frozen serialisation of `globalThis` plus
   well-known objects; every layout change requires a corresponding
   serialisation-schema change. Phase 4 (macros) will rewrite how
   intrinsics are *declared*, which transitively changes which
   objects show up in the snapshot graph. Designing the schema
   before the declaration surface stabilises means designing it
   twice.

2. **GC body schema churn.** Phase 1.2 moved most hot-path bodies
   into the GC (BigInt, ArrayBuffer, DataView, TypedArray,
   string Stage 1, closure / upvalue). Symbol and Temporal payloads
   are still on the foundation form pending follow-up. Snapshot
   restore must reconstruct each body's exact byte layout; freezing
   that layout while migrations are open burns review cycles on
   schema fix-ups.

3. **Performance signal not yet earned.** Bootstrap currently
   allocates ~1130 GC objects in the default feature set
   (`MAX_DEFAULT_GC_ALLOCATIONS = 1130` in
   `crates/otter-vm/src/bootstrap.rs`). A snapshot eliminates that
   work, but `cargo run --release -p otterjs -- -e 'null'` is
   already under 50 ms cold-start on a 2024 Apple Silicon dev
   machine. There is no production runtime where bootstrap time
   is the dominant cost today. JIT (blocked on Phase 4) and
   inspector tooling (Phase 5) are the higher-leverage wins.

## Prerequisites

The snapshot pipeline becomes designable only after **all** of
these hold. Each is a hard gate, not a "nice-to-have".

### P1 — `otter_*` macros (Phase 4) shipped and load-bearing

- Task 4.1 lands, intrinsic declarations migrate through 4.2.
- The macro-generated descriptor shape is the canonical input the
  snapshot serialiser walks. Designing the schema against the
  hand-written `BuiltinIntrinsic` adapters would lock the schema
  to a soon-to-be-deleted API.
- Specific gate: every entry in `BOOTSTRAP_ENTRIES` is generated
  by the new macros, *or* the holdouts are explicitly listed as
  "stays hand-written" in the macro RFC.

### P2 — GC body schema frozen

- Symbol and Temporal payloads migrated into GC bodies (carry-over
  from 1.2). At time of writing the Temporal payload still lives
  on the foundation form; `crates/otter-vm/src/temporal/payload.rs`
  is the migration target.
- `SafeTraceable` derive (Task 6.3) shipped so per-body schemas
  are mechanically derivable rather than hand-written. Without
  the derive the snapshot serialiser would re-implement the same
  field walks every body already maintains for tracing.

### P3 — Realm intrinsic slots stable

- The set of `%X%` slots in
  [`crates/otter-vm/src/realm_intrinsics.rs`](../crates/otter-vm/src/realm_intrinsics.rs)
  is closed, or its growth policy is documented. The snapshot
  must record every slot identity; an open-ended slot table means
  the snapshot version bumps every time a new well-known intrinsic
  is added.

### P4 — Bytecode wire format frozen at v2 + ratchet test

- The PIC slot table (Task 2.4 ✓), `OP_BYTE_TABLE` (Task 2.1 ✓),
  and `RealmIntrinsics` (Task 3.2 ✓) are already at v2. Add a
  format-version constant + `cargo test` ratchet that fails when
  any byte in the table shifts. Without this ratchet a future
  refactor can renumber an opcode and silently invalidate every
  shipped snapshot.
- Recommended: introduce `BYTECODE_FORMAT_VERSION` (different from
  the one Task 2.3 deleted — that was a per-module header constant
  with no readers; this one is a workspace-wide snapshot identity
  marker). The two roles do not collide.

### P5 — Cold-start regression budget defined

- Measure the actual baseline (`cargo run --release -p otterjs -- -e 'null'`
  median of 100 runs) and decide the target. If the budget is, e.g.,
  "<5 ms cold start", justify why the snapshot is the right tool
  vs. lazy-init of placeholder slots (`Intl`, `Temporal`,
  `AggregateError` already use this pattern).

## Acceptance triggers

The "ready to design" signal is the conjunction:

- P1 + P2 + P3 + P4 all green.
- A concrete embedder use case asks for sub-ms cold start
  (per-request JS isolates, edge-compute style workloads).
- Owner sign-off on schema bump as a versioned ABI commitment —
  every snapshot version is supported until the embedder migrates,
  and migration is on the consumer.

Until that conjunction holds, snapshot work stays in this
"deferred" state.

## What changes when we resume

A future RFC-4 (snapshot pipeline) will pick up from this point.
Its scope:

1. Schema design — node types, edge types, version field, endian /
   alignment commitments.
2. Serialiser — walks `globalThis` + every realm intrinsic slot
   + module environments + microtask queue (which must be empty
   at snapshot time).
3. Deserialiser — mmaps the blob, relocates internal pointers via
   the GC cage base, re-installs realm intrinsic slots, validates
   the format version, fails fast on schema mismatch.
4. Build pipeline — `mksnapshot` binary that runs bootstrap,
   serialises, writes to `target/<profile>/otter-startup.bin`.
   `otterjs` defaults to loading it; `--no-snapshot` falls back to
   the live bootstrap path for debugging.
5. CI — every PR must rebuild the snapshot and assert byte-for-byte
   reproducibility against a checked-in golden hash so schema drift
   is caught at PR time.

None of these subtasks have current owners. They wait.

## Cross-references

- [`docs/architecture-refactor-plan-2026-05.md`](architecture-refactor-plan-2026-05.md)
  — Task 3.3 plus Phase 4 / 5 / 6 follow-ups that gate this work.
- [`docs/native-call-abi.md`](native-call-abi.md) — frozen ABI
  that the macro layer (P1) must continue to generate.
- [`crates/otter-gc/src/snapshot.rs`](../crates/otter-gc/src/snapshot.rs)
  — in-memory walker; not the startup snapshot but reuses the
  same heap traversal primitives the serialiser would need.
- [`crates/otter-gc/src/devtools_snapshot.rs`](../crates/otter-gc/src/devtools_snapshot.rs)
  — Chrome DevTools JSON writer; precedent for "frozen format the
  GC must keep compatible".
- [`crates/otter-vm/src/bootstrap.rs`](../crates/otter-vm/src/bootstrap.rs)
  — the work this snapshot would replace; `MAX_DEFAULT_GC_ALLOCATIONS`
  ratchet documents the cold-start allocation cost.
