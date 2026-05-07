# Startup Performance

Otter treats startup as a production requirement. Contributor-friendly APIs
must not silently make `RuntimeBuilder::build()` or first script execution
slower.

The active benchmark suite covers
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

- `crates/otter-vm/benches/bootstrap.rs`;
- `crates/otter-runtime/benches/startup.rs`;
- `crates/otter-cli/benches/cold_start.rs`.

Bootstrap telemetry should be opt-in and default-off. Useful counters are:

- objects/functions/prototypes installed;
- strings interned;
- GC allocations and bytes;
- per-bootstrap-phase timing;
- duplicate-name and install-order validation cost.

When changing bootstrap, builders, macro generation, or builtin install
order, include a before/after startup table in the PR or closeout notes.

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

The local 2026-05-06 ratchet values are:

- default global bootstrap: ~113 us;
- global bootstrap with telemetry: ~121 us;
- `RuntimeBuilder::build()` default / production-sandbox: ~121 us;
- `Otter::builder().build()`: ~422 us, including isolate runner startup;
- first `run_script("undefined;")`: ~126 us;
- first extracted static native `Math.abs` call: ~122 us;
- CLI cold `-e ""` / tiny JS / tiny TS: ~25-26 ms.

The CLI cold-start bench also includes bucket cases:

- `info`: process + clap + dispatch baseline with no runtime/compiler touch;
- `dump_tiny_js_file`: compile-only first-touch frontend/compiler baseline.

The compile-only path is split from the runtime path. `otter check` and
`otter --dump-bytecode` call the compiler directly and do not construct a
`Runtime`, interpreter, or GC heap. Ambiguous `.js` / `.ts` file execution uses
one OXC parse for module-syntax detection and script compilation; `.mjs` /
`.mts` route directly to the module graph and `.cjs` / `.cts` route directly to
script execution.

After the cage-reuse fix, `cargo bench -p otter-runtime --bench
startup -- --sample-size 10 --measurement-time 2 --warm-up-time 1` completed
without an iteration cap. Its build-body-only smoke values were:

- `RuntimeBuilder::build()` default / production-sandbox: ~20 us;
- `Otter::builder().build()`: ~266 us, including isolate runner startup;
- first `run_script("undefined;")`: ~22 us;
- first extracted static native `Math.abs` call: ~26 us.

After the compile-only split and ambiguous-file parse-once routing,
`cargo bench -p otter-cli --bench cold_start -- --sample-size 10
--measurement-time 2 --warm-up-time 1` produced:

- `info`: ~3.14-3.18 ms;
- `eval_empty`: ~25.62-25.79 ms;
- `tiny_js_file`: ~25.24-25.59 ms;
- `dump_tiny_js_file`: ~3.19-3.26 ms;
- `tiny_ts_file`: ~25.85-26.61 ms.

Set `OTTER_CLI_STARTUP_TIMINGS=1` on a single CLI invocation to print
default-off phase timings to stderr. This is intended for cold-start triage,
not for benchmark scoring.

After removing eager zeroing of the full 256 MiB GC cage at process startup,
`cargo bench -p otter-cli --bench cold_start -- --sample-size 10
--measurement-time 2 --warm-up-time 1` produced:

- `info`: ~3.17-3.21 ms;
- `eval_empty`: ~4.31-4.35 ms;
- `tiny_js_file`: ~4.36-4.51 ms;
- `dump_tiny_js_file`: ~3.25-3.39 ms;
- `tiny_ts_file`: ~4.45-4.53 ms.

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
