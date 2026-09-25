# Allocation-free root scopes

Investigation and closing validation: September 25, 2026.

This is a local Apple M1/macOS AArch64 release investigation. The engine base
is the signed commit named in the environment file. Development observations
are not a published engine baseline.

## Starting point

A `sample` profile of `string-concat` on ProductionTiered spent about 30% of
its time in `malloc`/`free`. Every `RootScope` boxed a provider object and grew
a fresh slot `Vec` from zero capacity, so opening one scope and rooting two
values cost two heap allocations and two frees. The allocating string-concat
stub opens one scope and `JsString::concat` opens another, which made four
allocations and four frees per `+` on top of the string body itself. The same
pattern sits behind 102 `RootScope::new` sites in the VM.

## Change

- `FrameRootProviders` stores `Option<*const dyn FrameRoots>`: `None` marks an
  open root scope. The scope's slots live in one registry-owned stack, each
  tagged with the index of its owning marker.
- `RootScope` is two words, the heap pointer and its marker index. Adding a
  slot pushes onto the registry stack; dropping the scope truncates providers
  and pops every trailing slot owned at or above that index.
- Both stacks keep their capacity, so a steady-state scope allocates nothing.
  Tracing visits providers first and then every scoped slot, during both minor
  and major collections and in the eager root snapshot.
- A debug assertion rejects an outer scope adding a slot while an inner scope
  is open, which is the one ordering the owner-tagged truncation cannot
  represent. No VM site does this.

## Final paired result

Three directly alternating pairs per kernel and tier, eight warmups and twenty
samples, pair 2 reversed. The before binary is the previous slice's gate build;
the after binary is this slice's closing-gate build. All 27 processes validate
their checksums. Load average was about 3.1; no builds, tests or agents ran.

| Kernel / tier | Pair 1 before → after, ms | Pair 2 | Pair 3 | Wall | Instructions | Cycles |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| string-concat / Interpreter | 48.677 → 44.148 | 48.941 → 43.760 | 48.891 → 43.859 | **−10.06%** | **−8.84%** | −9.51% |
| string-concat / Template | 19.342 → 14.663 | 19.421 → 14.452 | 19.788 → 14.486 | **−25.53%** | **−37.23%** | −26.08% |
| string-concat / Production | 19.275 → 14.763 | 19.471 → 14.512 | 19.470 → 14.654 | **−24.54%** | **−37.38%** | −24.83% |
| native-boundary (control), three tiers | | | | −0.31% to +0.18% | ±0.03% | |
| polluted-feedback (control) / Template, Production | | | | +0.15%, +0.09% | −0.03%, −0.05% | |
| polluted-feedback (control) / Interpreter | 25.988 → 25.662 | 25.930 → 25.869 | 25.882 → 28.334 | +2.55% | −0.01% | +2.60% |

The control kernels open no root scope on their hot paths and retire the same
instructions. The Interpreter `polluted-feedback` change comes from one
outlying pair-3 process at identical instruction counts; pairs 1 and 2 are
flat. It is scheduling noise, and no claim is made. The JIT tiers keep the
same 5,600,000 allocating-stub calls per 28 invocations. A general-path call
now performs no `malloc`/`free` pair instead of four; the GC string body is its
only allocation.

At **61 hand-written production Rust lines added** (26 removed, excluding
comments and tests), the ProductionTiered wall gain is **0.40 percentage points
per added line**.

## Validation

- Full `CARGO_INCREMENTAL=0 CARGO_BUILD_JOBS=2 bash scripts/gate.sh` passed on
  the final source: fmt, warnings-denied Clippy, unit tests, release
  verifier/adversarial corpus, 42/42 differential cases, and the kernel ledger.
  The `compile_fail` snapshots for `GcHeap`'s `Send` explanation were
  regenerated: the trait chain now names the slot stack.
- `otter-gc` (57) and `otter-vm` (977) unit tests pass in debug builds with the
  slot-ordering assertion enabled.
- 21 runtime binaries covering GC builders, rooting, string ropes, string
  intrinsics, the Template differential, generic operators and coercion pass
  60/60 at `OTTER_GC_STRESS` unset, 1 and 16.
- `git diff --check` passed.

## Reproduction

```sh
/usr/bin/time -l <binary> kernel \
  --source benchmarks/scripts/string-concat.js \
  --function engineKernel --expected 600000 \
  --jit-tier <interpreter|template|production-tiered> \
  --samples 20 --warmup 8
```

[Environment](2026-09-25-allocation-free-root-scopes-environment.json),
[run counters](2026-09-25-allocation-free-root-scopes-runs.csv) and
[wall samples](2026-09-25-allocation-free-root-scopes-samples.csv) are tracked
here. Binaries and logs are retained under ignored
`benchmarks/results/session-2026-09-25b/`.
