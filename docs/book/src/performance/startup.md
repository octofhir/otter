# Startup Performance

Otter treats startup as a production requirement. Contributor-friendly APIs
must not silently make `RuntimeBuilder::build()` or first script execution
slower.

Task 98 owns the startup ratchet. The active benchmark suite covers
bootstrap install, runtime construction, first execution, static native
dispatch through an extracted builtin, and CLI process cold start.

## Local Workflow

Run:

```bash
cargo bench -p otter-vm --bench bootstrap -- --sample-size 30 --measurement-time 2 --warm-up-time 1
cargo bench -p otter-runtime --bench startup -- --sample-size 30 --measurement-time 2 --warm-up-time 1
cargo bench -p otter-cli --bench cold_start -- --sample-size 10 --measurement-time 2 --warm-up-time 1
```

Benchmark sources:

- `crates-next/otter-vm/benches/bootstrap.rs`;
- `crates-next/otter-runtime/benches/startup.rs`;
- `crates-next/otter-cli/benches/cold_start.rs`.

Bootstrap telemetry should be opt-in and default-off. Useful counters are:

- objects/functions/prototypes installed;
- strings interned;
- GC allocations and bytes;
- per-bootstrap-phase timing;
- duplicate-name and install-order validation cost.

When changing bootstrap, builders, macro generation, or builtin install
order, include a before/after startup table in the task closeout or PR.

The report should include:

```text
case                              before        after         delta
RuntimeBuilder::build(default)    ...           ...           ...
RuntimeBuilder::build(prod)       ...           ...           ...
first run_script("undefined;")    ...           ...           ...
CLI cold empty JS                 ...           ...           ...
CLI cold tiny TS                  ...           ...           ...
```

Always note the build profile, machine, command, sample count, and whether
the run used bootstrap telemetry.

The runtime startup bench uses a custom Criterion timing loop: each
iteration measures only the build/first-run body, then immediately drops the
runtime before the next iteration. This keeps the process-global GC cage
reused without batching thousands of live heaps or timing teardown. The
regression test
`cargo test -p otter-runtime repeated_otter_build_drop_returns_gc_pages_to_cage`
guards the same lifecycle at the public `Otter` handle boundary.

## Current Budgets

Task 98's local 2026-05-06 ratchet values are:

- default global bootstrap: ~113 us;
- global bootstrap with telemetry: ~121 us;
- `RuntimeBuilder::build()` default / production-sandbox: ~121 us;
- `Otter::builder().build()`: ~422 us, including isolate runner startup;
- first `run_script("undefined;")`: ~126 us;
- first extracted static native `Math.abs` call: ~122 us;
- CLI cold `-e ""` / tiny JS / tiny TS: ~25-26 ms.

After Task 99's cage-reuse fix, `cargo bench -p otter-runtime --bench
startup -- --sample-size 10 --measurement-time 2 --warm-up-time 1` completed
without an iteration cap. Its build-body-only smoke values were:

- `RuntimeBuilder::build()` default / production-sandbox: ~20 us;
- `Otter::builder().build()`: ~266 us, including isolate runner startup;
- first `run_script("undefined;")`: ~22 us;
- first extracted static native `Math.abs` call: ~26 us.

Bootstrap telemetry budget:

- duplicate registry names: `0`;
- bootstrap string interning: `0`;
- namespace objects: `4`;
- static native functions installed from specs: `57`;
- GC allocation delta: `<= 160`;
- GC live-byte delta: `<= 96 KiB`.

## Regression Policy

High-level contributor APIs must compile down to the same runtime shape as
handwritten static specs. Startup regressions need an explicit production
justification, a benchmark table, and a follow-up plan when accepted.

Lazy or tiered initialization must preserve spec-observable behavior. If a
surface's property enumeration, identity, or initialization timing would
change, do not lazy-install that surface without a spec note and tests.
