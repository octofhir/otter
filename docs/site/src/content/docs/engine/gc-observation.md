---
title: "GC observations"
---

`Runtime::start_gc_pause_capture(capacity)` enables an optional bounded capture
of synchronous collection service. `Runtime::take_gc_pause_capture()` transfers
owned scalar records. Use these methods at a quiescent runtime boundary; they
expose no object pointers, handles or root visitors. Capacity is 1 through 65,536
records and is reserved before JavaScript runs. Disabled capture reads no
observation clock. Collection never grows the record buffer or formats output.

A service interval includes collector setup, root processing, collection,
cleanup and the trigger's GC accounting tail. A full interval includes its
nested minor collection. Trigger metadata comes from the entry owner:
explicit, stress, nursery capacity, heap cap, growth budget or code reclamation.
The records also preserve full/minor cycle counters before and after service.
Completed collection followed by an allocation refusal is distinct from a
collector failure.

These intervals measure collection service. Allocation retry, generated caller
register restoration, later finalization jobs and concurrent collector work
are outside that definition. They are insufficient to assert a matched whole
mutator pause comparison with another engine.

Capture overflow, incomplete transfer, collector failure and split mark/step/
weak/sweep service remain explicit metadata. Split calls may have mutator work
between them and must not be presented as one whole-cycle pause. Missing
collection classes have no tail values; an absent full collection is never a
zero-nanosecond full pause.

## Resource probe

The resource probe uses the existing production runtime and one realm. Its init
script is compiled, linked and executed once, then the probe resolves one plain
JavaScript function or closure from the exact `globalThis` property named by
`--function` (default `engineKernel`). One strong persistent root retains that
callable through all warm, measured and retention phases. Every invocation
checks its actual function ID and calls the same root with no arguments and
`undefined` as `this`; replacing the global property cannot replace the kernel.
The root is removed on completion or failure. No VM value/closure handle is
stored in host probe state.

Build the optional consumer with the engine feature:

```sh
cargo build --release -p otter-benchmark --features engine --bin otter-resource-probe
```

An init script supplies the unchanged kernel itself, for example a benchmark's
existing function assigned to `globalThis.engineKernel`. All work/input and
retained-owner/reset semantics belong to that source. The probe does not parse,
rewrite, wrap or manufacture a JavaScript workload.

```sh
target/release/otter-resource-probe kernel.js \
  --function engineKernel --expected 41 --warmup 100 \
  --trace fresh-natural-gc.trace.json \
  --validate-retained retained-check.js --expected-retained 41 \
  --release release-owners.js --expected-released 0
```

`--setup` optionally executes once in the same realm before init. `--expected`
is a finite numeric checksum; every warm/measured result must match its exact
binary64 value, including the sign of zero. Non-numeric, NaN, infinite, abrupt,
missing-result and process-exit completions fail. Tier options are `interpreter`,
`template`, and default `production`, using their normal runtime policies.
Owned before/after generation rows identify the kernel's actual function ID;
generation presence alone is not an assertion of native execution or warm-tier
readiness. A benchmark gate must still establish its intended native policy.

Before natural capture, the probe completes warm calls and one forced-full
baseline. Capture covers exactly one subsequent invocation of the retained
callable, including its normal native-event microtask checkpoint. It contains
no source compilation/linking, forced baseline/retention collections, script
validation, trace formatting or RSS inspection. Stress must be absent or zero;
an observed explicit/stress trigger also fails the natural-scope proof.

The trace target must be fresh. The standard Chrome Trace Event artifact keeps
every captured raw interval, exact nanoseconds and cycle metadata before a
measured checksum, coverage or retention failure can terminate the probe.
Sequence must be exact, intervals ordered/nonoverlapping, counters nonregressing
and continuous. Summed completed full and all minor deltas, including nested
minors in each outer full record, must match the runtime's exact before/after
GC counters. Dropped/incomplete/split/failed service remains unscoreable.
Unavailable or invalid tail summaries have null quantiles/max and explicit
gaps. An absent class is never assigned a zero percentile or manufactured work.
`status: passed` validates execution and capture coverage; availability of both
class distributions is reported separately by `naturalPauseTailCoverageComplete`.

After capture and raw trace export, the probe drops its native observation and
trace tree, forces full GC, and records retained components. Optional
`--validate-retained` runs after that full-GC snapshot and requires its paired
`--expected-retained` completion string. Optional `--release` requires the paired
`--expected-released`, then another forced full GC supplies released components.
Both actual phase completions remain in the owned result, including mismatches.
The kernel's persistent root and init execution context remain live in all three
component snapshots so their lifetime is consistent.

Live GC cells, allocated-space capacity, all off-slot reservations and RSS stay
separate. Native allocator pages released after trace formatting can remain in
RSS; RSS is an absolute process component and not managed retained live bytes.
Matched retained recipes, paired empty controls and binary/source identity gates
are still required externally. The probe has no validated equivalent Bun
whole-mutator pause boundary and reports that comparison as unavailable.
