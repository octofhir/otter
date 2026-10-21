# Otter JIT — next session: call/frame contract (and remaining gaps)

Work autonomously toward JIT performance parity with V8/JSC/SpiderMonkey.
Big steps, modeled on those engines; research their design before choosing
one. If you hit a session limit, set a timer and continue when it resets.

## Rules (non-negotiable)
- Work on `main` only: no branches, no worktrees, no stash; never
  `git checkout`/`restore` over WIP files; never touch or commit
  `PROMPT_*.md` files.
- Commits must be GPG-signed (`git log --show-signature -1` shows `G`); NO
  Co-Authored-By or any AI trailer.
- Run tests/gates first and read their result; `git commit` goes in a
  SEPARATE call. Never chain a commit after tests with `;` or pipes.
- No feature flags, env toggles, adapters, compatibility shims; delete
  replaced code. No `thread_local`/`static mut`. No debt notes or phase
  labels in comments or commits; comments are timeless. New/changed Rust
  modules keep accurate `//!` docs (purpose, `# Contents`, `# Invariants`,
  `# See also`).
- Never `cargo test --workspace`; at most 1–2 `--test` runtime binaries.
  One test262 runner at a time, `--timeout 30000`. No cargo while taking
  fixed-work measurements. Delete `*.rcgu.o` after big builds
  (`find target -name "*.rcgu.o" -delete`); disk is ~95% full.
- Results/scripts live in the repo (`benchmarks/results/…` is gitignored,
  `scripts/dev/…`), never `/tmp`.

## Measure and gate
- Wall clock is the product metric — measure it every slice, not just
  instructions: `hyperfine -N -w 1 -r 5 "target/release/otter run <f>" "node <f>"`
  (medians; results in `benchmarks/results/walltime-*/`). Instructions alone
  hide main-thread vs background work (V8 compiles and collects off-thread).
- Fixed work (instructions, for attributing changes):
  `python3 scripts/dev/fixed-work.py benchmarks/results/<dir> --engines otter --workloads earley-boyer ts crypto zlib fib ast_ctor mega_method`
  and compare stdout files against a previous run's `*-otter.stdout`.
- Profiles: `sample <pid> <secs> 1 -file f.txt` + `python3 scripts/dev/selfprof.py f.txt 40`;
  JIT regions: `DELAY=… SECS=… python3 scripts/dev/hotasm.py <script> <outdir> 3`
  (long runs: `benchmarks/results/profiling/{crypto-long,fib-long}.js`).
- Gates before commit: `target/release/otter-difftest` (85/85),
  `target/release/otter-test262 run --filter language/ --timeout 30000`
  (24077 passed, 0 failed), `--filter built-ins/` (23520 passed; only the
  two RegExp `RGI_Emoji*` budget failures), GC stress
  `python3 scripts/dev/gc-stress.py target/release/otter <out> <corpus…> --strides 1-16`,
  `cargo test -q -p otter-jit --lib` (302) and
  `cargo test -q -p otter-jit --lib --target x86_64-apple-darwin machine` (193),
  `cargo test -q -p otter-vm --lib` (991) when the VM changes.
  Smoke scripts vs Node in all tiers (`--interpreter`, `--jitless`, default):
  `scripts/dev/{ctx_alloc_smoke,closure_alloc_smoke,oddball_arith_smoke,int_truncation_smoke}.js`.

## State (HEAD `de0784c6`, 2026-09-29)
Landed this series (all gated, signed):
- `625b5e82` contexts and closures carved from the linear allocation buffer
  in both tiers (plans in the compile snapshot; Machine
  `CallTarget::ContextAllocation`, `ClosureAllocationProbe`).
- `bb18373a` Number probe decodes undefined/null/Booleans (zlib −15%).
- `b4413eb1` deopt-direct `ElementCheckedLoad`/`ElementCheckedAddress`,
  hoistable dense `ElementProof`, Word32 truncation of checked int +/−,
  GVN int32 identities (zlib −22%, crypto −16%).
- `b8b96e6d` ToInt32 via FJCVTZS when the CPU has JSCVT (crypto −3.8%).
- `de0784c6` architecture report §13–§14
  (`benchmarks/measurements/2026-09-27-architecture.md`), incl. measured
  non-levers — read §14 before choosing work.

Now vs Node 24 (HEAD `de0784c6`; wall = hyperfine median of 5):

| Workload | Otter wall | Node wall | wall gap | Otter instr | Node instr | instr gap |
|---|---:|---:|---:|---:|---:|---:|
| ts-fixed | 22.7s (min 18.9) | 1.21s | **18.8x** | 173.4G | 21G | 8.3x |
| earley-boyer | 7.74s | 1.13s | 6.9x | 111.5G | 18.7G | 6.0x |
| zlib-fixed | 9.93s | 2.18s (`--no-validate-asm`) | 4.6x | 154.7G | 35.1G | 4.4x |
| ast_ctor | 1.43s | 0.255s | 5.6x | 21.7G | 3.3G | 6.6x |
| crypto-fixed | 0.82s | 0.166s | 4.9x | 13.7G | 2.9G | 4.7x |
| fib | 0.23s | 0.073s | 3.1x | 3.64G | 0.95G | 3.8x |
| mega_method | 0.476s | 0.234s | 2.0x | 10.0G | 4.0G | 2.5x |

ts is the worst: Otter runs it single-threaded (18.1s user, 54G cycles,
IPC 3.2, RSS 419 MB, high run-to-run variance); Node spends 2.55s user in
1.34s real — V8 compiles (Maglev/TurboFan) and marks/scavenges on
background threads. Main-thread work, not just instructions, is the gap.
(zlib is asm.js: Node runs it as wasm in 1.58s by default.)

## Next: the call/frame contract (report §5 slice 3)
A generated call costs ~200 instructions (fib: 437 per invocation vs V8's
114). No single part dominates — activation-array publication, per-call
`JitMachineRootRecord` push/pop and root spills, callee window fill with
`undefined`, caller-side tier counters (1.6% of fib alone), sloppy-`this`
global lookup, entry-cell `ldar`, stack/limit checks, `NativeResultPair`
status dispatch. Only the structural redesign removes them together:
fp-chained frames found by stack walk, safepoint data per frame/return
address, no per-call publication, callee-side tier budget, lazy/inline
frame descriptions from deopt data, status only on runtime transitions.

1. Read `benchmarks/measurements/research-2026-09-29/call-frame-contract.md`
   and `…/call-frame-consumers.md` (consumer map of every reader/writer of
   NativeFrame, activation array, root records, JitCtx and statuses: GC
   roots, deopt, stack traces/profiler, runtime stubs/`RuntimeCall::bind`,
   Template and Machine frames, exceptions, entry/lifetime, tier-up/registry).
   If `call-frame-consumers.md` is missing, re-run that map first (one
   read-only reader per area plus a completeness critic).
2. Write the design into the report (compare V8 frames/safepoint tables/lazy
   deopt, JSC call frames/OSR exit, SpiderMonkey JitFrames), with stages that
   each ship measurably: e.g. (a) intrusive frame chain replacing the
   activation array + per-frame root fields replacing per-call root records;
   (b) callee-side tier budget, no window fill for Machine callees, lazy
   sloppy `this`; (c) fp-walk consumers (GC, stack traces, deopt) and remove
   publication; (d) residual calls inside spliced bodies with inline frames
   described by deopt data (physical publication measured as a loss).
3. Implement stage by stage with full gates and GC stress after each.

## Also on the table (wall-clock driven)
- Concurrent compilation: Machine compiles run on the main thread (ts ~6%
  of samples in regalloc/HIR/GVN; zlib ~11% before the element work). V8
  compiles off-thread and installs at a safe point; Otter snapshots are
  moving-GC-stable DTOs, which is the precondition.
- Parallel/concurrent GC work (V8 parallel scavenge, concurrent marking).

## Other evidenced levers
- earley: `instanceof` target proof (closure → rare → bag → exotic →
  shape → slot, ~12 dependent loads) ≈13% of the hot functions; scavenges
  ~10%. A constant-prototype dependency needs code-dependency invalidation
  with on-stack stops (none exists: invalidated code keeps running).
- ast_ctor: a closure owning a property bag (assigned `prototype`) has no
  IC; `_super.call` resolves `call` by spelling each time (~28%).
- crypto `am3`: index `j` arrives as a double (per-access conversion);
  int-mul −0 checks.
- ts: JIT 40%, runtime property paths 18% (megamorphic keyed/named/store
  misses, shape-chain walks), call machinery 10%, allocation 7%, Machine
  compile 6%.

## Traps learned
- `benchmarks/results/` is gitignored; `hotasm` with `--jit-artifacts`
  distorts early windows (artifact writing) — sample late or use plain
  `sample` for tier splits.
- Octane zlib is asm.js: compare with `node --no-validate-asm`.
- `tbnz`/`tbz` reach only ±32 KiB — never branch to deopt trampolines with
  them; use conditional branches (±1 MiB).
- Location inputs are late uses (never in clobbered registers); register
  inputs are early — load them first in emitters that use fixed scratch.
- Deopt ids must be unique per instruction; one instruction owns an exit.
