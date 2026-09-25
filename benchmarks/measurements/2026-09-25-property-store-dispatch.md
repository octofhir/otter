# Fused property store dispatch

Investigation and closing validation: September 25, 2026.

This is a local Apple M1/macOS AArch64 release investigation. Development
observations are not a published engine baseline.

## Starting point

A per-region profile of `derived-constructor` on ProductionTiered showed each
`this.x = …` in the derived constructor compiled as independent CacheIR
chains: `GuardShape`, `GuardExtensible`, a `LoadPrototype` +
`GuardShape` pair per prototype link, `GuardPrototypeNull`, `StoreField` and
`PublishShape`. Every step repeated the complete receiver or holder decode
(cell test, cage add, body type, shape-state bytes) and materialized a
Boolean between steps. About 330 instructions per add transition went to
guards. V8's Maglev `StoreMap`/`TransitionElementsKind` path and
SpiderMonkey's Warp `AddAndStoreSlot` prove the transition's receiver map
once and walk the prototype chain against recorded maps with one load each.

## Change

- A new Machine operation, `PropertyStoreDispatch { byte_pc, cases }`,
  replaces every store program of a site that `store_case` recognizes: an
  existing writable own slot, or an own-data add whose prototype chain ends in
  null. Each case carries the receiver shape, the value slot, and for a
  transition the prototype shapes, child shape, new length and inline flag.
- AArch64 and x86-64 emission decode the receiver once, select the case by
  hidden class, walk the prototype contract with one decode per link, check
  extensibility, append position and capacity, then store and publish the
  child shape. Outputs are the stored owner, the published child and a hit
  flag; on x86-64 they are written in a cycle-safe order.
- Selection follows the dispatch with the value barrier and, when the site
  has a transition, the child-shape barrier. A miss has written nothing and
  enters the committed cold store. Constructor field stores use the same
  single-case dispatch under `GuardCondition` with their frame state, which
  removes the separate guard-chain builder.
- Effects: reads shape, property metadata, property fields and prototype
  state; writes property fields, shape and property metadata; never commoned.
- `code-map.json` names the region `machinePropertyStoreDispatch`.

## Final paired result

Three alternating pairs, eight warmups and twenty samples, pair 2 reversed.
All processes validate their checksums.

| Kernel / tier | Pair 1 ms | Pair 2 | Pair 3 | Wall | Instructions | Cycles |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| derived-constructor / Production | 8.015 → 6.770 | 7.998 → 6.762 | 8.029 → 6.763 | **−15.58%** | **−18.48%** | −14.99% |
| derived-constructor / Template | | | | +0.06% | +0.03% | +0.17% |
| derived-constructor / Interpreter | | | | +0.33% | +0.02% | +0.32% |
| property-polymorphic, method-call-monomorphic, dense-array, string-concat (controls), JIT tiers | | | | −0.37% to +0.46% | ≤0.18% | |

Template and the interpreter do not use the new operation.

At **628 hand-written production Rust lines added** (176 removed, 452 net),
the ProductionTiered `derived-constructor` gain is **0.025 percentage points
per added line** (0.034 per net line).

## Validation

- Full gate (fmt, warnings-denied Clippy, unit tests, release verifier).
- `otter-jit --lib machine`: 176 AArch64, 171 x86-64 under Rosetta, including
  new selection tests for mixed existing/transition sites.
- Runtime JIT/GC set at `OTTER_GC_STRESS` unset/16/4/1: 169/169 on AArch64;
  154/157 on x86-64. The three x86-64 failures are
  `jit_stack_owned_runtime_families`, where x86-64 Template cannot compile the
  callees. They fail the same way without this change.
- Targeted Test262 on ProductionTiered, both architectures, zero failures:
  property accessors, class expressions and statements, `new`, `super`,
  `for-in`, `Object.defineProperty`, `Object.create`, Proxy `get`/`set`,
  calls and assignment (10 804 tests each).

[Environment](2026-09-25-property-store-dispatch-environment.json),
[run counters](2026-09-25-property-store-dispatch-runs.csv),
[wall samples](2026-09-25-property-store-dispatch-samples.csv).
