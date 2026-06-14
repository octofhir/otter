# benchmarks

Cross-runtime JS/TS performance baseline for **Otter** vs **Node**, **Deno**, **Bun**.

Purpose: capture a baseline **before** perf work and the JIT, so every later
change can be measured against it. The same scripts double as **debugging /
dump** repros — where Otter fails or hangs today, that's recorded baseline truth.

Pure-JavaScript workloads only — no `fs`/`net`/Node APIs. Just language + core
builtins (Array, JSON, RegExp, TypedArray, Math, classes, strings).

## Layout

```
benchmarks/
  bench.mjs        harness (Node) — runs every script across every runtime, times it
  scripts/         workloads (*.js / *.ts); files starting with _ are ignored
  results/         latest.md + latest.json (overwritten each run)
```

## Run

```bash
# build the engine first (harness looks for target/{release,debug}/otter)
cargo build --release -p otter-cli

# all scripts, all detected runtimes
node benchmarks/bench.mjs

# subset of scripts (substring match) / runtimes
node benchmarks/bench.mjs fib nbody
node benchmarks/bench.mjs --only otter,node

# knobs
node benchmarks/bench.mjs --runs 20 --warmup 3 --timeout 30000
node benchmarks/bench.mjs --json /tmp/run.json
```

Runtimes are auto-detected from `PATH` (`node`, `deno`, `bun`) and the built
`otter` binary. Missing ones are skipped.

## Metric

**Min wall-clock ms** over N runs (default 10, 2 warmup), lower is better.
Min is the cleanest single number — least polluted by OS scheduling noise.

Timing **includes process startup**, deliberately: startup is part of real-world
runtime cost. For compute-heavy scripts startup is a small fraction; for cheap
scripts it dominates and that's worth seeing too.

The `×` columns are the ratio vs Otter (`>1` = slower than Otter, `<1` = faster).

## Adding a benchmark

Drop a `*.js` / `*.ts` file in `scripts/`. Rules:

- Pure JS/TS — no Node/Deno/Bun-specific APIs.
- Print **one** final result line (a checksum) so output can be diffed across
  runtimes for correctness.
- Size it so the slowest runtime finishes in a few seconds, not minutes.
- Prefix with `_` to exclude from runs.

## Bugs this baseline surfaced (all fixed, 2026-06-14)

Building these benchmarks exposed three real engine bugs, since fixed and
verified against test262 (JSON, Object, Array, Map, class — no regressions):

1. **`JSON.stringify` use-after-move** — the spec serializer held `Value`s
   across GC-allocating calls. Fixed with a scratch GC-root stack
   (`json_root_stack`, a manual HandleScope) plus a no-alloc fast path for
   enumerable key reads.
2. **Prototype-chain corruption** — object construction captured the
   prototype handle *before* the allocation that could trigger a scavenge,
   then wrote the stale (moved) pointer as `[[Prototype]]`. Corrupted
   inheritance / `JSON` / `instanceof` for objects built across a GC. Fixed
   in `Object.create` and the object-literal opcode (read the prototype
   after the alloc).
3. **Young-gen retention OOM** — retaining a nursery-sized live set
   (~20–30k objects) deadlocked the copying scavenger and surfaced a
   spurious out-of-memory while the cage was 95% free. Fixed by overflowing
   such allocations into old-space.

Pre-JIT, Otter runs ~10–190× slower than V8/JSC — that gap *is* the baseline
this directory exists to measure and shrink.
