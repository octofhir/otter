# PROMPT_DOMINATE — rebuild the engine until it beats node and bun everywhere

Goal: otter beats node AND bun on every axis, measured, no exceptions:
wall-clock on every `benchmarks/fixtures/fixed-work/` fixture, peak RSS,
GC pause times, startup (`--version`, `scripts/dev/empty.js`,
`scripts/dev/hello.js`, a script touching `process.stdout`), and package
install time (bun, pnpm, and nub as the Rust-tooling reference). This is the
only goal. The user wants results, not reports.

Work autonomously. Do not ask questions, do not stop at a checkpoint, do not
hand back. On a session limit, schedule a wakeup and continue from the
memory notes.

## What "hard refactor" means here (non-negotiable)

- Every slice is a whole-subsystem change ported from a reference design:
  read the subsystem in `~/work/engines` (v8, WebKit, gecko-dev, graal)
  FIRST, then port its architecture. Never name those engines in code,
  comments, identifiers or commits.
- Banned: chains of 1–3 % micro-tunings, per-probe special cases,
  instrument-and-patch loops, bench-fitting, "try a constant and measure".
  A small change counts only as part of a structural slice.
- Delete the old path in the same slice. No flags, no env toggles, no
  compat bridges, no second copy, no versioned formats.
- Profiling only orders the work; the design comes from the reference.
- If a slice regresses anything, fix it forward in the same slice.
- All failures are ours. No "pre-existing" or "test rot" verdicts.

## Loop

1. Measure: `just release` (writes the startup image), then
   `scripts/dev/wallclock_matrix.sh target/release/otter benchmarks/results/<slice>/m 5`
   plus the startup set with `hyperfine -N -w 20 -r 200` against node and bun.
2. Take the biggest gap by absolute cost (time × ratio). Classify it with
   `sample` + `scripts/dev/isolate_self.py`, `scripts/dev/sample_kind_prof.py`
   (node kinds in optimized code), `scripts/dev/xctrace_top.py` (launch
   profiles; copy the binary under a unique name to filter).
3. Read the reference implementation of that subsystem end to end.
4. Port it fully, delete what it replaces.
5. Gates (below), one GPG-signed commit, memory note, next gap.

## Standings (2026-10-20)

Matrix m3, × node / × bun: regexp 3.35/3.99 (now ~3.0 after 4c66b5289),
ts 2.78/3.22, deltablue 2.27/1.66, box2d 2.27/2.51, gbemu 2.09/–,
splay 2.00/2.41, raytrace 1.55/2.92, mandreel 1.53/2.14, earley 1.42/1.37,
crypto 1.32/–, richards 1.29/1.52, navier 1.25/–, zlib 1.09/–.
Geomean 1.76 node / 2.28 bun. RSS over node: regexp 148/76, box2d 145/98,
gbemu 161/121, mandreel 263/132, zlib 124/83, ts 632/504.
Startup (with image, load ~7): `--version` 4.2 ms (bun 4.1), empty.js
7.6 (bun 6.8), hello.js 10.6 (bun 6.8), `process.stdout` touch ~19
(bun 13.7, node ~29).

## Slice queue, biggest first

1. **Optimizing-tier code quality** (deltablue, box2d, raytrace, gbemu, ts
   spend ~65–82 % in generated code). Port in full: register allocation
   with live-range splitting and spill placement, polymorphic inlining
   behind map checks (no generic-call fallback killing LICM), unboxed
   double representation end to end, bounds-check elimination and
   induction-variable analysis, loop peeling/unrolling where the reference
   does it, store/load elimination across calls with effect summaries,
   allocation folding and wider escape analysis, boolean field categories.
   Measure per node kind; the target is the reference engine's instruction
   count for the same hot loops (`deltablue Plan.execute`, box2d solver).
2. **Generic native-call ABI from optimized code** (47 ns per builtin call
   vs node 1–5): direct call into the native with an exit frame and the
   arguments in place (no host frame publication, no register-window
   fill), plus graph lowering of common builtins (Object.is,
   Number.isInteger, hasOwnProperty, Array.isArray, String methods,
   Math) to inline code.
3. **RegExp pipeline**: exec/test/replace/split/match all as direct
   entries; last-match info written by the matcher itself; result arrays
   with in-object index/input/groups; replace and split building strings
   in one pass; per-pattern matcher parity with the reference regex
   compiler (test with the `scripts/dev/probe/regexp_*.js` probes).
4. **ts**: megamorphic property stub cache, dictionary/hashtable objects
   in generated code (keyed loads without the runtime), compile throughput
   (the compiler's own CPU is wall time), tier-up policy.
5. **GC**: concurrent marking (design in memory
   `gc_concurrent_marking_design`), old-space compaction and page release
   (splay committed ≫ live), pretenuring that tracks allocation sites,
   pause-time targets per collection kind. Report pauses with `--trace-gc`.
6. **Memory**: mandreel compile peak (streaming parse/compile instead of a
   whole-file AST), regexp/box2d/gbemu RSS sources, image page sharing.
7. **Startup, continued**: the image restore costs ~1.2 ms per MB today.
   Decode functions lazily, restore pages at their captured offsets so no
   relocation walk runs, map image pages instead of copying. Console with a
   native fast path for primitive arguments; `process.stdout` without
   loading the whole stream stack up front; nodelib modules decoded per
   function on first call; binding fixups at launch.
8. **Package manager**: warm and cold install against bun/pnpm/nub on the
   same lockfile; fix the stray-package bug seen in a fresh dir
   (dunder-proto manifest parse, unrelated packages pulled in).

## Hard rules (from memory; break none)

- Work only in main: no branches, worktrees, stash, checkout or reset
  over WIP. Undo your own edit with its exact reverse. Never touch foreign
  untracked files.
- Commit with
  `git -c user.name="Alexander Streltsov" -c user.email="funyloony@gmail.com" commit`;
  signature must show `G`. No Co-Authored-By or AI trailer. No
  "remaining / not done" text in commit messages. Commit in a separate call
  after reading the test output.
- Files in the repo (`scripts/dev`, `benchmarks/results/<slice>`), never
  /tmp. No design-doc files. Comments timeless, no phase labels.
- No `thread_local!`, `static mut` or process-global state; no Rc
  crutches; handle scopes for multi-allocation natives; host closures
  hold no `Value`; per-run native state goes in host services.
- New or changed Rust modules get `//!` docs (purpose, Contents,
  Invariants, See also). Extend existing tests; new tests only for new
  mechanisms.
- Never `cargo test --workspace`; at most two `--test` targets per run.
  One test262 runner at a time. Keep >15 GB disk free; clean stale target
  dirs after commits.
- `cargo build --release -p otter-cli` drops the startup image; run
  `just release` before measuring or running difftest.

## Gates per slice

- `cargo test -q -p otter-vm --lib`, `-p otter-jit --lib` (also
  `--target x86_64-apple-darwin`), crate tests of every touched crate.
- `cargo run --release -q -p otter-difftest`, and with
  `OTTER_GC_STRESS=300full OTTER_GC_VERIFY=1` and
  `OTTER_GC_STRESS=7 OTTER_GC_VERIFY=1`: 182/0 each, on `just release`.
- `cargo clippy -p <touched crates> --all-targets -- -D warnings`, host
  and x86.
- `bash scripts/test262-full-run.sh > test262_results/full-run-<date><n>.log`
  (background, long timeout), then
  `python3 scripts/dev/t262_failset.py test262_results/latest.json benchmarks/results/parity-2026-10-19/t262-fail-base.txt`
  must print "new 0".
- Commit message: before/after wall-clock and instructions per affected
  fixture, load average, the gate results.

Start now: finish gating the startup-image slice in the working tree
(commit message draft `benchmarks/results/startup/image-commit-msg.txt`),
commit it, then take slice 1.
