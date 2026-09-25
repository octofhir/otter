# Polymorphic own-data property loads

Investigation and closing validation: September 25, 2026.

This is a local Apple M1/macOS AArch64 release investigation. Development
observations are not a published engine baseline.

## Starting point

`property-polymorphic` retired about 707 instructions per loop iteration on
ProductionTiered. The Machine artifacts showed three megamorphic probes and a
four-way load site lowered as four independent CacheIR chains. Each chain
repeated the complete receiver decode (cell test, cage add, body type, shape
state bytes) in every `CacheIrGuardShape`, `CacheIrGuardAtomSlot` and
`CacheIrLoadField`, materialized a Boolean between them, and ran for every
program even after an earlier one hit: 28 + 24 + 27 instructions per program,
about 320 per iteration for that one site. V8's
`LoadPolymorphicTaggedField` (Maglev) and SpiderMonkey's `GuardMultipleShapes`
(Warp) decode the receiver once and dispatch on its map.

## Change

- A new Machine operation, `PropertyPolymorphicLoad { byte_pc, cases }`,
  replaces every own-data program of a load site: a `GuardShape`, an optional
  read-only `GuardAtomSlot` naming the same slot, and a `LoadField`, all on the
  receiver. The same `own_data_slot` recognizer serves monomorphic speculation.
- AArch64 and x86-64 emission decode the receiver once, check the shape-state
  bytes once, load the hidden class once, compare it with each case, check
  ordinary lookup state only for cases that need it, and read the slot. A miss
  returns undefined/false into the existing committed cold load.
- Its operands match `PropertyMegamorphicLoad`; effects read shape, property
  metadata, property fields and prototype state with value commoning; GVN
  canonicalizes the case list. Prototype and other programs keep their chains.
- `code-map.json` names the region `machinePolymorphicPropertyLoad`.

## Final paired result

Three alternating pairs, eight warmups and twenty samples, pair 2 reversed.
Before = previous slice's gate build, after = this series' closing gate build
(the same build carries the standby-page, x86 clobber and relink changes, none
of which this kernel exercises: it allocates about 1.6 KB and makes no calls).
All processes validate their checksums; load average about 2.3.

| Kernel / tier | Pair 1 ms | Pair 2 | Pair 3 | Wall | Instructions | Cycles |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| property-polymorphic / Production | 12.085 → 9.666 | 12.117 → 9.647 | 12.073 → 9.699 | **−20.02%** | **−21.25%** | −19.18% |
| property-polymorphic / Interpreter | | | | +0.15% | +0.00% | |
| property-polymorphic / Template | 76.941 → 82.330 | 77.906 → 90.137 | 77.222 → 81.177 | +9.18% | +0.04% | +6.36% |
| method-call-monomorphic, dense-array (controls), JIT tiers | | | | −0.22% to +0.24% | ≤0.01% | |

Template does not use the new operation; its instructions are identical, and
the wall change is a code-placement effect in the reentrant property stubs it
spends its time in. No claim is made for it.

At **182 hand-written production Rust lines added** (4 removed), the
ProductionTiered gain is **0.11 percentage points per added line**.

## Validation

- Full gate (fmt, warnings-denied Clippy, unit tests, release verifier,
  differential 42/42, kernel ledger); `otter-jit --lib machine` 175 (AArch64) /
  170 (x86-64), including a new selection test for a three-way site.
- 46 runtime binaries (property, inline, speculative, direct-call, GC-builder
  and element families) pass on AArch64 at `OTTER_GC_STRESS` unset, 16, 4 and 1
  and under Rosetta, except the x86-only `jit_stack_owned_runtime_families`
  cases that need x86 Template opcode coverage (tracked separately).
- Structural fixtures that counted `CacheIrLoadField` now accept the
  polymorphic region or count its cases.

[Environment](2026-09-25-polymorphic-property-loads-environment.json),
[run counters](2026-09-25-polymorphic-property-loads-runs.csv),
[wall samples](2026-09-25-polymorphic-property-loads-samples.csv).
