# Object Internal-Method Vtable — Decision Record

Refactor plan reference:
[`docs/architecture-refactor-plan-2026-05.md`](architecture-refactor-plan-2026-05.md)
§Task 6.1. Plan acceptance explicitly allows "no correctness
regression; property / proxy benchmarks improve, **or task is
rejected with data**".

This document records the rejection with the data behind it and the
trigger conditions that would re-open the work.

## Status — DEFERRED 2026-05-24

Not implemented. Re-evaluate when the IC-miss rate or the per-kind
chain branch count moves into the windows documented under
"Re-trigger conditions" below.

## Background

The VM dispatches the spec-shaped object internal methods
(`[[Get]]`, `[[Set]]`, `[[Has]]`, `[[Delete]]`,
`[[DefineOwnProperty]]`, `[[GetPrototypeOf]]`,
`[[SetPrototypeOf]]`) through hand-written `if let Some(x) =
base.as_X()` chains inside `crates/otter-vm/src/object_internal_ops.rs`
and `crates/otter-vm/src/property_dispatch.rs`. Each chain enumerates
every per-kind exotic shape — ordinary object, proxy, array, function
/ closure, native function, bound function, class constructor, regexp,
typed array, array buffer, data view, promise, map / set / weak-map /
weak-set, boolean / number / symbol box, string box, big-int, …

A JSC-style static internal-method table indexed by
`Traceable::TYPE_TAG` would replace the chain with one indirect
call.

## Inventory

Worst-case chain depth observed in the active code base:

| Function | Per-kind branches |
| -------- | ----------------- |
| `ordinary_get_value` | 13 |
| `ordinary_has_property_value` | 12 |
| `ordinary_set_data_value` | 11 |
| `vm_delete_property` (and helpers) | 10 |
| `ordinary_get_own_property_descriptor_value_runtime_rooted` | 11 |

Hot path: every chain places `if let Some(obj) = base.as_object()`
first, so the most common ordinary-object receiver pays one branch.
The remaining branches matter only for non-ordinary receivers and
for the corner cases where the receiver is a primitive (string box,
boolean box) or a callable.

## Measurement

`crates/otter-vm/benches/property_ic.rs` covers the load / store /
has / delete fast and slow paths. Baseline (the 1k-iteration cases
captured during this evaluation):

| Bench | Median |
| ----- | ------ |
| `named_delete_own_data_1k` | 569.75 µs |
| `named_delete_missing_1k` | 467.72 µs |
| `named_delete_inherited_present_1k` | 328.99 µs |

That comes out to ~470–570 ns per delete on a one-kind workload — a
ceiling case for the dispatch chain because IC does not cover the
delete bytecodes. The load / store / has families flow through the
property inline cache before they reach the chain at all; the
PIC dump already shows healthy polymorphic-to-megamorphic coverage
(Phase 5.2 acceptance, `inspect_snapshots::ic_snapshot_reports_polymorphic_and_megamorphic_states`).

The conservative read of those numbers:

- IC fast path absorbs the majority of `[[Get]]` and `[[Set]]` work
  for ordinary objects. The chain only runs on a guard miss, a
  prototype walk, or a non-object receiver.
- The dispatch chain pays ~5–15 ns per skipped `as_X` check
  (sub-tag inspection plus a GC header read once the sub-tag matches
  pointer-bearing). With 13 branches worst case, that is at most a
  ~150–200 ns ceiling on the chain itself, but typical workloads see
  one to three branches before the right arm fires.
- A vtable swap removes only the branch-walk overhead, not the
  per-kind body. Realistic upside on the slow path is in the
  1–3 % range; on the full mixed workload that translates to a
  fraction of a percent.

## Implementation cost

The migration would touch the entire spec-internal-method surface:

- `object_internal_ops.rs` (≈3k LOC) — every `ordinary_*` body
- `property_dispatch.rs` (≈3k LOC) — every opcode wrapper
- `proxy.rs`, `array.rs`, `array_prototype.rs`,
  `arguments_object.rs`, `bound_function.rs`,
  `class_constructor.rs`, `regexp.rs`, `regexp_prototype.rs`,
  `string.rs`, `collections.rs`, `weak_refs.rs`,
  `typed_array/*`, `data_view.rs`, `array_buffer.rs`,
  `promise.rs`, `generator.rs` — every per-kind handler

ABI shape: each table cell is an `fn(&mut Interpreter,
&ExecutionContext, JsObject-ish handle, …)` — but the handle type
differs by kind. A uniform vtable forces every cell to take a
`Value` and re-cast inside, which gives up exactly the
specialisation the chain produces today.

Effort estimate: M+ (multi-week). Correctness risk: medium — the
internal-method surface is the hottest spec-conformance area; every
arm has subtle invariants around proxy traps, typed-array
out-of-bounds, arguments-object backing, etc.

## Decision

**Reject for the current refactor cycle.** Net expected improvement
on a representative JS workload sits well under the 5 % floor the
plan principles call out for high-risk surface rewrites
([`docs/architecture-refactor-plan-2026-05.md`](architecture-refactor-plan-2026-05.md)
§Principles, "every task has an acceptance signal"). The
implementation cost is large and the risk-adjusted upside is small.

## Re-trigger conditions

Re-open this task when **any** of the following lands:

1. **IC hit rate drops below 80 %.** Until then the PIC dominates the
   hot path and chain savings stay invisible. Track via
   `PropertyIcStats::load_hits / (load_hits + load_misses)` over a
   real workload sample.
2. **Average chain branch depth rises above 4.** If new exotics are
   added in front of `as_object` (or if a future opcode forces
   slow-path dispatch for ordinary objects), the per-call branch
   count moves into the window where a vtable is worth the rewrite.
   Instrument the chain with a sticky counter under
   `#[cfg(feature = "vm-dispatch-counters")]` before the next
   measurement pass.
3. **A measurable Proxy benchmark regression.** `built-ins/Proxy`
   test262 conformance is at 219/311 (per the active refactor plan
   open-task list); future Proxy-heavy workloads might push the
   chain branch count high enough that the rewrite pays off.

Each re-trigger criterion must be backed by a current measurement
attached to the issue that re-opens the task.

## See also

- [`docs/architecture-refactor-plan-2026-05.md`](architecture-refactor-plan-2026-05.md)
  §Task 6.1.
- [`crates/otter-vm/src/object_internal_ops.rs`] — current chain.
- [`crates/otter-vm/src/property_dispatch.rs`] — opcode wrappers
  that reach the chain.
- [`crates/otter-vm/benches/property_ic.rs`] — baseline benchmark.
- [`crates/otter-vm/src/inspect.rs`] — `ic_snapshot()` /
  `IcSiteState::{Polymorphic, Megamorphic}` (Phase 5.2) — the
  primary signal used to detect the IC-hit-rate trigger above.
