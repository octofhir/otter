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

## Standings (2026-10-25)

Matrix (HEAD e2fe4329f plus the pending slice below), × node / × bun:
regexp 3.08/3.63, ts 2.58/3.02, deltablue 2.17/1.51, splay 2.13/3.52,
gbemu 1.92/–, richards 1.66/1.98, earley 1.40/1.34, raytrace 1.38/2.50.
Older m1 (35747b87f): geomean 1.73 node / 2.29 bun. Absolute ms
(otter/node): ts 3120/1211, regexp 1271/413, richards 1065/643,
gbemu 916/476, deltablue 613/282, splay 670/315.

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
  `OTTER_GC_STRESS=7 OTTER_GC_VERIFY=1`: all pass (197/0 today), on
  `just release`.
- `cargo clippy -p <touched crates> --all-targets -- -D warnings`, host
  and x86.
- `bash scripts/test262-full-run.sh > test262_results/full-run-<date><n>.log`
  (background, long timeout), then
  `python3 scripts/dev/t262_failset.py test262_results/latest.json benchmarks/results/dominate-1024/t262-fail-base.txt`
  must print "new 0" (base: 15 failing).
- Never edit sources or rebuild `target/release` while a background gate
  runs: the gates execute that binary.
- Commit message: before/after wall-clock and instructions per affected
  fixture, load average, the gate results.

## State 2026-10-25

### Landed since 2026-10-21 (all `G`, no trailer)

- 3adf18741 named loads on a symbol walk Symbol.prototype.
- c5719b074 GC helpers join a parallel job only while it is open.
- 9baef67c7 absent array element reads complete in generated code.
- 35747b87f Array push/pop trust the %Array.prototype% chain proof
  (deltablue −5%).
- 77aef68c0 slot write finds its field summary in one read (gbemu −9%).
- 210af4283, 0c8c52f47 operands held across allocation/user code are
  re-read from roots.
- bf00202c2 numeric appends stay in generated code; storage grows by half.
- cf97b53bd every GC root list (`value_roots`/`slice_roots`) is
  `&mut [&mut Value]`: a root passed as `&Value` let release builds fold the
  caller's re-read to the pre-move address (stale String-wrapper data).
- e2fe4329f natives take `args`/captures as `ValueWindow<'_>` (raw start +
  length, `Deref<Target=[Value]>`), not `&[Value]`: the same miscompile at
  native-ABI scale. Natives, helpers, macros and `NativeCallRoots` use it.

### Pending slice (uncommitted in the tree when this prompt was written)

Graph generic keyed access plus constructor feedback, files:
`crates/otter-jit/src/graph/{arm64,x86_64}/keyed_{load,store}.rs`,
`graph/ir.rs`, `crates/otter-vm/src/call_ops/constructor.rs`,
`crates/otter-difftest/corpus/keyed_store_appends.js` (generic-site block),
`scripts/dev/jit_callees.py`, `scripts/dev/jit_self_by_fn.py`.
- `LoadKeyedCached` reads a hole or an index past the length as
  `undefined` inline while the array-index protector holds and the array
  has no sidecar.
- `StoreKeyedCached` grows a full array on append through
  `ARRAY_PUSH_ALLOC` at a safepoint (refusal → runtime with parked
  operands, OOM → resume exit).
- `note_published_constructor_receiver` skips construct-family feedback
  when the caller frame is `Optimizing`.
Effect: optimized-frame keyed stub calls on ts −60% (holes 158k → 5k,
appends 300k → 28k), megamorphic-constructor probe 1333 → 1105 ms; ts
wall within noise. Gates were running at handoff: if the tree still holds
these files uncommitted, run the full gates, then commit them as one slice.

### Findings that order the next work

- ts profile (isolate self): generated code 58% (flat over hundreds of
  functions, optimized 46% / template 9.5%), interpreter entry 6.4%,
  keyed load stub 4.2%, keyed store stub 3.5%, host native calls 3.7%,
  `promote_entered_function` 3.4%, `alloc_group_ensure` 1.9%,
  constructor receiver commit 1.4%, GC ~7%, compile on isolate ~8%.
- Remaining keyed traffic on ts, nearly all from optimized frames:
  - shaped receivers with atom keys: 322k loads, 106k stores; all cold
    `(shape, atom)` pairs, from small fast-mode objects used as symbol
    tables (60% absent or inherited);
  - arrays with index-like string keys (`"37"`): 175k loads. The graph
    keyed load sends any non-number key on an array straight to the
    runtime;
  - dictionary objects with int keys: 76k loads.
  The no-allocation leaf `keyed_named_load_leaf` refuses shaped receivers
  on purpose (the runtime miss fills the shared table; skipping that fill
  once measured slower for hot pairs). Count distinct pairs first: unique
  pairs want a leaf answer (generic keyed loads in the reference engine do
  not fill the stub cache); repeating pairs want eviction fixed.
- Megamorphic store (`StorePropertyCached`) is the top optimized node kind
  on ts: ~40 ns per store, the shared-table probe plus the field-type
  observer branches. Constructor receivers come from the callee prologue
  (two calls: probe leaf + commit), so `this` has no proved shape and
  stores cannot lower to transitions. Tried and dropped: baking shared-table
  store programs per (site, receiver shape). It costs 50–100 ms of bake on ts
  (scans all 8192 ways per site) and never fires without a `this` shape
  proof. Needs a per-atom index and a constructor `this` shape proof first.
  Probe: `scripts/dev/probe_megamorphic_ctor*.js` (otter ~290 ns per
  construction, node ~60, bun ~21).
- regexp per op vs node: exec miss 43/13.5 ns, test 29/11.5, url-groups
  299/112, trim replace 313/174; Rust layering, not the matcher.
- Open in the same miscompile class: `GcTrace::trace_gc_roots(&self)` and
  other callee-side traces write through shared references (low risk:
  nothing re-reads across them).
- Do not redo: numeric field representation via migrations / read
  canonicalization / universal Double (rejected by whole-run CPU,
  `benchmarks/measurements/2026-10-22-engine-architecture-audit.md`).

### Tools

- `scripts/dev/jit_callees.py <sample.txt>`: isolate self samples by the
  first native callee below generated code.
- `scripts/dev/jit_self_by_fn.py <sample.txt> <perf.map>`: generated-code
  self samples per compiled function and tier (`--perf-basic-prof`).
- `scripts/dev/sample_kind_prof.py`: per graph node kind (artifact writing
  distorts absolute shares on big fixtures; rankings hold).
- `sample <pid> <secs> 1 -file f.txt` sometimes attaches before the isolate
  thread exists: retry until the isolate sample count is large.
- `OTTER_STATS=1` prints `jitReentrantStubTransitions` and friends.
- Rewrite scripts driven by rustc JSON diagnostics must restrict edits to
  repository paths (one run once edited `~/.cargo/registry` smallvec; fix:
  delete the extracted registry dir and rebuild).

### Measurement and housekeeping

- Retired-instruction counts vary about 3% run to run; compare medians of
  five. `/usr/bin/time -l` prints instructions retired.
- Disk: ~47 GiB free; `cargo clean` after commits when target exceeds
  ~40 GB.
