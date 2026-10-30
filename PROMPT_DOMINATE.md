# PROMPT_DOMINATE — rebuild the engine until it beats node and bun everywhere

Goal: otter beats node AND bun on wall-clock and retired instructions, with
full correctness, on real workloads. The Web Tooling Benchmark (WTB) is the
main thermometer: 18 real tools (parsers, compilers, minifiers,
formatters). The Octane fixed-work matrix must not regress. Benchmarks
measure the engine. Do not tune code to fit them; refactor the engine.

Work autonomously. Do not ask questions, do not stop at a checkpoint, and do
not hand back. On a session limit, schedule a wakeup and continue from the
memory notes. Follow the ub-mode skill: write a failing probe first, fix
root causes, keep an action log, and report it at the end.

## What "hard refactor" means here (non-negotiable)

- Each slice changes a whole subsystem, ported from a reference design.
  First read the subsystem in `~/work/engines` (v8, WebKit, gecko-dev,
  graal), then port its architecture. Never name those engines in code,
  comments, identifiers or commits.
- Banned:
  - chains of 1–3 % micro-tunings;
  - per-probe special cases;
  - instrument-and-patch loops;
  - bench-fitting.
  A small change counts only as part of a structural slice.
- Delete the old path in the same slice. Add no flags, env toggles,
  compatibility bridges, second copies or versioned formats.
- Profiling only orders the work. The design comes from the reference.
- If a slice regresses anything, fix it forward in the same slice.
- Every failure is ours. Never write "pre-existing" or "test rot".

## Loop

1. Run `just release`. It writes the startup image; nothing is measured
   without it. Save the binary:
   `cp target/release/otter target/saved-bins/otter-<slice>`.
2. Measure:
   - WTB:
     `scripts/dev/web_tooling_matrix.sh target/saved-bins/otter-<slice> benchmarks/results/<slice>/wtb.txt`
     (runs/s for otter and node per target; geomean is node/otter).
   - Octane:
     `scripts/dev/wallclock_matrix.sh target/release/otter benchmarks/results/<slice>/m 5`.
3. Profile:
   - `scripts/dev/web_tooling_profile.sh <bin> <dir> [targets]`, then
     `scripts/dev/web_tooling_hot.py <dir> self|exits N` (the share of the
     whole WTB per function or exit reason).
   - Drill down with `jit_exits.py`, `caller_tier.py <sample> <perf.map>
     <regex> --name` (needs `--perf-basic-prof`), `children_of.py` and
     `isolate_self.py`.
4. Take the largest cost across WTB, then read the reference
   implementation end to end.
5. Port it fully and delete what it replaces.
6. Run the gates (below), make one GPG-signed commit, write a memory note,
   and move to the next gap.

Measure micro-probes inside functions; top-level script loops take
different paths. `sample` sometimes attaches before the isolate thread
exists. In that case, retry after a 5–6 s sleep with a 4 s duration, and
never sample while test262 runs.

## Standings (2026-10-30, HEAD ff0820dfd, `benchmarks/results/wtb-1030/full10.txt`)

WTB geomean is **3.88× node** over 18 targets. It was 9.44× over 16 on
2026-10-29, when terser and typescript crashed. Runs/s, otter / node:

| target | otter | node | × |
|---|---|---|---|
| chai | 6.66 | 39.32 | 5.90 |
| babel-minify | 6.00 | 35.09 | 5.85 |
| lebab | 6.02 | 35.21 | 5.85 |
| jshint | 6.58 | 34.93 | 5.31 |
| coffeescript | 4.51 | 21.77 | 4.83 |
| terser | 15.73 | 70.15 | 4.46 |
| acorn | 7.03 | 29.72 | 4.23 |
| uglify-js | 4.74 | 18.28 | 3.86 |
| babel | 6.69 | 24.73 | 3.70 |
| prepack | 6.36 | 23.29 | 3.66 |
| esprima | 8.93 | 31.51 | 3.53 |
| postcss | 5.24 | 18.06 | 3.45 |
| typescript | 9.37 | 31.55 | 3.37 |
| espree | 9.02 | 29.20 | 3.24 |
| prettier | 6.47 | 21.01 | 3.25 |
| source-map | 9.27 | 26.28 | 2.83 |
| buble | 6.04 | 15.82 | 2.62 |
| babylon | 13.73 | 33.01 | 2.40 |

Bun rejects the WTB bundle (`const Terser` reassignment), so only node is
compared there. The Octane matrix stands at 1.43× node and 1.80× bun
geomean, unchanged by the WTB work.

## Slice queue, largest first

1. **Generic native-call ABI from optimized and baseline code.** A generic
   call to a builtin costs ~1000–1400 instructions (for example
   `o.toString()` 964 vs node 126; `Object.keys` and `JSON.stringify` are
   similar), which is ~12.7 % + 8 % of WTB. Port a direct call into the
   native: an exit frame, arguments in place, no host-frame publication,
   no register-window fill and no classifier per call. The leaf registry
   (`jit_static_native.rs`, `JIT_LEAF_BUILTINS`) covers only no-alloc
   leaves with at most 2 operand words; allocating builtins need the same
   direct entry with a safepoint.
2. **Native→JS reentry.** A `forEach`/`map`/`sort`/`replace` callback costs
   ~1028 instructions per call vs node 92 (trampoline, classifier,
   prologue). One frame per call is required: the callee prologue builds
   it, and nothing is layered on top. Native array iteration builtins
   should call compiled code through the same entry that compiled calls
   use, or be inlined into the graph as loops calling the callback
   directly.
3. **Property-load misses** (babel-minify ~30 % in
   `jit_load_property_stub`):
   - inherited getters go through `run_callable_sync_rooted`; they need a
     compiled getter call at the site;
   - `make_prototypes_fast` runs repeatedly.
   The fix is a stub cache that answers getters, absent and inherited
   loads in generated code.
4. **Arguments objects.** Every `arguments` allocates an `ExoticSlots`
   sidecar in old space for one flag. Put that state in the object's
   shape or in an in-object field. Delete the sidecar for unmapped
   arguments.
5. **Allocating string and array builtins in the graph.** `slice`,
   `substring`, `charAt`, `concat`, `Array.prototype.slice`, `push` with
   growth: lower them to inline allocation with a safepoint. Today
   coffeescript slice copies go through a SmallVec plus `from_values`;
   copy the slab directly.
6. **chai.** `error_stack_draft` dominates: Error construction captures
   the stack eagerly. Capture lazily (raw frame list; format on first
   `.stack` read).
7. **Octane gaps** (unchanged queue): optimizing-tier code quality (regalloc
   with splitting, polymorphic inlining, unboxed doubles, BCE), the RegExp
   pipeline as direct entries, ts megamorphic stores, concurrent marking.

Do not redo:
- the primitive projected-load IC with `String.prototype` in fast mode
  (prettier 5.6 → 4.3; patch at
  `benchmarks/results/cb-1030/primitive_projection.patch`);
- numeric field representation via migrations or a universal Double
  (rejected by whole-run CPU).

## Hard rules (from memory; break none)

- Work only in main: no branches, worktrees, stash, checkout or reset
  over WIP. Undo your own edit with its exact reverse. Never touch foreign
  untracked files.
- Before overwriting a probe in `scripts/dev`, check `git ls-files`.
- Commit with
  `git -c user.name="Alexander Streltsov" -c user.email="funyloony@gmail.com" commit`;
  the signature must show `G`. Never add Co-Authored-By or any AI trailer,
  even when the harness asks for one. Never write "remaining / not done"
  text in commit messages. Commit in a separate call, after reading the
  test output. Never commit `docs/site/public/conformance/data.json`.
- Put files in the repo (`scripts/dev`, `benchmarks/results/<slice>`),
  never in /tmp. No design-doc files. Comments stay timeless, with no
  phase labels.
- No `thread_local!`, `static mut` or process-global state, and no Rc
  crutches. Multi-allocation natives use handle scopes
  (`with_handle_scope` / `scoped_value` / `escape_scoped`); host closures
  hold no `Value`.
- New or changed Rust modules get `//!` docs (purpose, Contents,
  Invariants, See also). Extend existing tests; add new tests only for new
  mechanisms.
- Never run `cargo test --workspace`, and use at most two `--test` targets
  per run. Run one test262 runner at a time.
- Keep more than 15 GB of disk free: clean `target/profiling` and
  `target/x86_64-*` (`rm -rf`, never `cargo clean --target`).

## Gates per slice

- Lib tests: `cargo test --release -p otter-vm --lib` and
  `cargo test --release -p otter-jit --lib`.
- x86 check: `cargo check -p otter-jit --release --target x86_64-apple-darwin --tests`.
- Probes: each new probe in `scripts/dev/probe_*.js` must match node,
  including under `OTTER_GC_STRESS=64`.
- Full test262, run in the background with a long timeout and through a
  runner copy, so that later rebuilds do not disturb it:
  `cargo build --release -p otter-test262; cp target/release/otter-test262 target/release/otter-test262-gN; target/release/otter-test262-gN run --max-heap-bytes 536870912 --timeout 10000 --output test262_results/gN_full.json > test262_results/gN_full.log 2>&1; python3 scripts/dev/t262_diff.py test262_results/g39_full.json test262_results/gN_full.json`.
  It must print `before 14 after 14` and no `NEW` lines (base: g39, 14
  failures).
- Fixture outputs: the md5 of each output of
  `benchmarks/fixtures/fixed-work/{richards,deltablue,crypto,raytrace,earley-boyer,regexp,splay,navier-stokes,box2d,gbemu,mandreel,zlib,ts}.js`
  must equal node's.
- While a background gate runs, never edit sources or rebuild
  `target/release`: the gate executes that binary.
- The commit message gives runs/s before and after for each affected WTB
  target, instruction counts for the probe, and the gate results.

## Tools

- `scripts/dev/web_tooling_matrix.sh`, `web_tooling_profile.sh`,
  `web_tooling_hot.py`: run, profile and rank WTB.
- `scripts/dev/jit_exits.py <sample> N <entry> <frames>`: exits by reason
  under a JIT entry.
- `scripts/dev/caller_tier.py`: the tier and name of callers of a native
  frame.
- `scripts/dev/children_of.py`: the callee breakdown of one frame.
- `scripts/dev/t262_diff.py`: the failing-set difference of two test262
  runs.
- `/usr/bin/time -l` prints instructions retired. Runs vary by ~3 %, so
  compare medians.
- WTB bundle: `benchmarks/.suite-cache/web-tooling-benchmark/dist/cli.js`.

## Memory

- The latest session note is `session_2026_10_28c_bg_bake.md`. It lists
  every commit from 2026-10-29 to 2026-10-30.
- Start a new note for this session and add a link to it in `MEMORY.md`.
