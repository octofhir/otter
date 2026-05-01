# ADR-0004 — GC Crate and Unsafe Boundary

- **Status:** accepted
- **Date:** 2026-05-02
- **Deciders:** project lead
- **Amends:** [`docs/new-engine/adr/0001-staging-directory.md`](./0001-staging-directory.md) §5
- **Related:**
  - [GC architecture plan](../gc-architecture.md)
  - [Task 70 — GC track master tracker](../tasks/70-gc-master-tracker.md)
  - [Task 71 — GC crate skeleton](../tasks/71-gc-crate-skeleton.md)
  - [Task 72 — core heap and handles](../tasks/72-gc-core-heap-and-handles.md)

## Context

ADR-0001 §5 reads:

> Every `crates-next/*` crate declares `#![forbid(unsafe_code)]`.
> Foundation phase does not introduce a new GC or JIT, so no
> exception is needed. If a future slice introduces
> `crates-next/otter-gc`, that slice amends this ADR explicitly.

Foundation phase is now closing. The next major track is the
production-grade tracing GC documented in
[`docs/new-engine/gc-architecture.md`](../gc-architecture.md). The
GC is a V8/JSC-shaped page-based generational collector with
pointer compression, semispace scavenger, tri-color marking, and
write barriers. Implementing it without `unsafe` is not feasible:
the design surfaces — page-aligned `mmap`, `GcHeader` atomic
flags, raw forwarding pointers, pointer compression cage — all
require it.

This ADR is the explicit amendment ADR-0001 §5 calls for.

## Decision

1. **Add `crates-next/otter-gc/` to the workspace `members` list.**
2. **Lift `#![forbid(unsafe_code)]` for `crates-next/otter-gc/`
   only.** Every other `crates-next/*` crate (`otter-bytecode`,
   `otter-syntax`, `otter-compiler`, `otter-vm`, `otter-runtime`,
   `otter-test`, `otter-test262`, `otter-cli`) keeps the workspace
   ban. Concretely:
   - The workspace `[workspace.lints.rust] unsafe_code = "forbid"`
     stays in place.
   - `crates-next/otter-gc/Cargo.toml` declares its own `[lints.rust]`
     section that overrides the workspace forbid (it omits the
     `unsafe_code` lint), keeping `missing_docs = "deny"` and the
     workspace clippy `all = "deny"`.
   - All other crates retain `[lints] workspace = true` and inherit
     the forbid as before.
3. **Hygiene constraints (mandatory; PR-review and CI-grep
   gated):**
   - Every `unsafe` block in `crates-next/otter-gc/` carries a
     `// SAFETY:` comment immediately above it stating the
     preconditions the caller relies on.
   - Every public `unsafe fn` documents preconditions in its
     docstring under a `# Safety` section.
   - Every non-trivial `unsafe` block has a corresponding miri
     test (`cargo +nightly miri test -p otter-gc`).
   - Module docstrings + ECMA-262 spec links per ADR-0001 §6 and
     [`docs/new-engine/tasks/README.md`](../tasks/README.md)
     §Working rules 6.
4. **Boundary inventory.** The unsafe surface is bounded to the
   GC crate's internals — page allocation, scavenger forwarding,
   mark-bit atomics, pointer compression cage. The public API
   surfaced to `otter-vm` and other crates remains safe Rust:
   `Gc<T>`, `Local<'gc, T>`, `HandleScope<'gc>`, `Traceable`,
   `GcHeap`, `OutOfMemory`, `GcStats`, `HeapSnapshot`. See
   [GC architecture plan §6.1](../gc-architecture.md) for the
   detailed boundary description.
5. **No path-dep on `crates/otter-gc/`.** The legacy GC crate is
   excluded from the workspace per ADR-0001 and remains so. The
   new crate reproduces the legacy *design* under the new
   conventions; no source-level imports, no `[dependencies]`
   path entries.

## Status of `crates-next/*` after this ADR

| Crate | `unsafe_code` |
|---|---|
| `otter-bytecode` | forbid |
| `otter-syntax` | forbid |
| `otter-compiler` | forbid |
| `otter-gc` | **permitted (this ADR)** |
| `otter-vm` | forbid |
| `otter-runtime` | forbid |
| `otter-test` | forbid |
| `otter-test262` | forbid |
| `otter-cli` | forbid |

## Consequences

- The GC track (tasks 70–91) can land its full design without a
  second ADR amendment until concurrent marking (Phase 4, task 87)
  forces a re-evaluation of barrier paths under multi-threading.
- PR-review checklist for any `crates-next/otter-gc/` PR: every
  new `unsafe` block has `// SAFETY:`; every public `unsafe fn`
  has `# Safety`; miri suite passes locally.
- The unsafe surface stays bounded — auditing focus stays on the
  GC crate's boundary, not spread across the workspace.

## Notes

- A future slice that introduces a JIT (`crates-next/otter-jit/`,
  not yet planned) will require its own ADR amendment with the
  same shape as this one — distinct from this decision.
- If concurrent marking lands (Phase 4, task 87), the barrier
  paths gain `Sync` requirements; an ADR-0005 amendment may be
  needed to widen the unsafe boundary or document new
  `Send`/`Sync` invariants.
