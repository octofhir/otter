# PROMPT_DOMINATE_2 — continue PROMPT_DOMINATE.md

The goal, the loop, the slice queue, the hard rules and the gates are those
of `PROMPT_DOMINATE.md`; all of it stays in force. This file holds the state
at 2026-10-21 07:00 MSK and the order of work from here. Work autonomously,
no questions, no hand-back; on a session limit schedule a wakeup and
continue from memory `session_2026_10_21_dominate`.

## Landed 2026-10-21 (all `G`, no trailer)

- b78119439 fix(vm): a baseline poll hand-off through a generated entry is
  no deopt.
- f705ee8dd perf(jit): `Math` methods as `Float64Math` (a `BranchKind::Native`
  diamond: the native arm computes inline, the other arm calls the replaced
  method, no deopt), int32 `/` and `%` by a constant (`graph/division.rs`),
  boolean `==` as `===`. box2d 658 → 597 ms, raytrace 830 → 783 ms.
- 7864e3077 perf(jit): a keyed store at an Array's length appends inline.
  gbemu 963 → 941 ms.
- 71bb068ed perf(jit): fold decides a branch below one edge of a dominating
  branch on the same test. No fixture moved.
- 3f49211f0 perf(jit): diamonds that only pick a value become selects
  (`graph/select.rs`, csel/fcsel). No fixture moved.

The standings in `PROMPT_DOMINATE.md` predate these commits.

## Order of work

1. **Verify x86.** `just release` first: `target/release/otter` is an
   ungated experiment build, not HEAD. x86 test binaries hung under Rosetta
   all night (even `--list` timed out after 30 s, processes stuck in state
   U), so the five commits above passed x86 clippy but never ran x86 tests,
   and they all emit x86 code (Select, Float64Math, the Native branch,
   keyed append, constant division). Run
   `cargo test -q -p otter-jit --lib --target x86_64-apple-darwin` and the
   otter-vm `jit_registry` tests on x86. If Rosetta still hangs, run them
   under Docker + qemu (memory `x86_kernels_run_under_docker_qemu`). Any
   failure is a bug in those commits: fix it forward.
2. **Correctness before speed.**
   - Difftest under `OTTER_GC_STRESS=300full`, `fresh_receiver_appends.js`,
     diverges now and then: the optimizing tier once kept appending its own
     `y` after a setter appeared on `Point.prototype`. Suspected cause: a
     background compile installed after its dependency was invalidated.
     Read the install path and make it re-check dependencies at install;
     do not loop reruns.
   - `require("internal/otter/net")` and the other `internal/otter/*`
     bindings load from user code; node refuses `internal/*`. Close this,
     and check that every such native enforces its capability at the Rust
     boundary.
   - `otter -p … | head` panics on EPIPE; node exits quietly.
3. **Fix the tty port** (0c56affaa, pushed on request before its full
   gates ran):
   - Its natives in otter-node `src/tty.rs` take any descriptor and check
     no capability (`_caps` is unused). User code can reach them through
     `require("internal/otter/tty")`, so it can write to, and switch the
     blocking flag of, any descriptor the process holds. Accept only the
     standard streams and descriptors the run opened through its
     capability-checked fs, enforced at the Rust boundary, with a test.
   - `process.rs` resets terminals on every exit through a hidden
     `__otter_reset_terminals__` property on `process`, which user code can
     see. Move it to an internal slot or a host service; the abort path in
     `process_control.rs` uses the same property.
   - Every program takes this exit path, so measure `hello.js` and the
     `process.stdout` startup case against
     `benchmarks/results/slice4/otter-select-v1`.
   - Gates it has not run yet: `cargo test -p otter-cli --test node_util`
     (debug build), the otter-node crate tests, and clippy on otter-node
     and otter-runtime, on the host and on x86.
4. **Re-measure.** Run the matrix against node and bun
   (`scripts/dev/wallclock_matrix.sh … 5`) and the startup set with
   hyperfine. `benchmarks/results/slice4/otter-select-v1` is HEAD's release
   binary, startup image included; use it as the A/B base. Record the
   standings in memory.
5. **Slice 5: optimizing-tier code quality** (queue item 1). Gaps from the
   box2d analysis, as shares of optimized time, still open:
   - no double field representation: `CheckedTaggedToFloat64` costs
     9 instructions per field read (11%);
   - property-adding stores (10%);
   - static function properties read through a 26-instruction
     `LoadFunctionBag` (5.5%);
   - block layout (4.5%);
   - register allocation (3%).

   The largest structural item is field representation:
   - each field has a representation in its map (none → smi → double →
     heap object → tagged);
   - generalizing a field deprecates the map and migrates instances;
   - double fields are stored unboxed, as the reference does;
   - optimized loads and stores trust the representation under a map
     dependency.

   Read the reference's field-representation and map-update code end to
   end before porting it.

   The select pass does not fire on deltablue's hot loop (12.15G retired
   instructions, unchanged by the pass). Before changing the select rules,
   dump the graphs with
   `target/release/otter --jit-artifacts=benchmarks/results/<slice>/artifacts benchmarks/fixtures/fixed-work/deltablue.js`
   and read what the `direction` diamonds of `input()`, `output()` and
   `Plan.execute` actually hold.
6. **Remaining gaps:** `Blob.prototype.stream` is missing; web
   `performance` lacks `mark` and `measure`;
   `runtime_handle_bounded_queue_reports_backpressure` is flaky under load
   (100 ms timeout).

## Measurement and housekeeping

- Retired-instruction counts vary about 3% from run to run (background
  compile timing). Compare medians of five runs:
  `RUNS=5 benchmarks/results/slice1/measure.sh <base> <new> [fixtures]`.
  Move the script to `scripts/dev/` in the next commit that uses it.
- Difftest now has 186 cases (the gates in `PROMPT_DOMINATE.md` say 182).
- Disk: 21 GiB free; `target/debug` is 34 GB. The 1.8 GB of comparison
  traces in `benchmarks/results/slice1-cmp/` can be deleted.
