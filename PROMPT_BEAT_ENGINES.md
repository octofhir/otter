# PROMPT_BEAT_ENGINES — beat node and bun on every metric

Goal: otter beats node AND bun on wall-clock speed, memory (RSS, heap
footprint), GC pauses and startup, on every fixture in
`benchmarks/fixtures/fixed-work/` plus `hello.js` / `empty.js`. The user
wants RESULTS, measured numbers, no discussion. Work autonomously and
without asking questions. Choose whole-subsystem rewrites over small tweaks,
and delete old paths instead of keeping them alongside.
Engine sources for reference designs are in `~/work/engines` (v8, WebKit,
gecko-dev, graal). Read and port their architecture, but never name them in
code, comments, identifiers or commit messages.

## Loop

1. Measure: `scripts/dev/wallclock_matrix.sh` (wall-clock vs node and bun).
   `scripts/dev/instr_ab.sh <a> <b> [runs] [fixtures]` (retired
   instructions) is a diagnostic only.
2. Pick the biggest gap by absolute cost.
3. Port the reference design and implement it fully.
4. Run the gates (below).
5. Make one GPG-signed commit per slice.
6. Go to the next gap.

On a session limit, schedule a wakeup and continue.

## State at handoff (2026-10-19 evening)

Last commit: 03e723d4e (int32 fields + exact int division).

Matrix m6 (350208a0c), × node:

| Fixture | × node |
|---|---|
| regexp | 3.29 |
| ts | 2.82 |
| deltablue | 2.41 |
| splay | 2.37 |
| gbemu | 2.06 |
| box2d | 1.95 |
| raytrace | 1.60 |
| mandreel | 1.57 |
| earley-boyer | 1.40 |
| crypto | 1.39 |
| richards | 1.28 |
| navier-stokes | 1.23 |
| zlib | 1.09 |

geomean 1.78 vs node, 2.44 vs bun.

RSS above node: ts 586/503, splay 578/392, gbemu 258/120, mandreel 263/132, regexp 197/76.

Startup: hello ~14 ms vs bun 7.6 ms.

### UNCOMMITTED, gated: copy-on-write array-literal boilerplates (finish and commit first)

**What it does:**
- **Bytecode:** `Op::NewArrayBoilerplate dst, const` and
  `Constant::ArrayBoilerplate { elements: Vec<BoilerplateElement> }`
  (crates/otter-bytecode).
- **Compiler:** emits the op for all-constant literals
  (crates/otter-compiler/src/expr/object_array.rs `boilerplate_elements`).
- **VM:**
  - `Interpreter::array_boilerplates` caches one shared old immutable
    slab per constant (literal_allocation.rs `array_boilerplate_slab`).
  - `array::from_shared_slab`.
  - The shared flag lives in `ElementSlabBody.shared`; every slab mutator
    asserts private.
  - `ensure_private_elements` copies out without collecting (young
    no-collect, else the new `GcHeap::alloc_old_trailing_no_collect`).
  - A shared array reports `dense_cap = 0`.
  - `dense_own_guard` has bit0 EXOTIC and bit1 SHARED.
- **JIT:**
  - Element reads test only bit0 (`JitBodyGuard.mask`/`clear_bit`).
  - Writes need the whole byte zero.
  - Holey writes also check the own byte.
  - Keyed loads use `tbnz` on bit0.
  - Inline pop checks the own byte; the push grow path clears it.
  - The graph inline shell allocation is `emit_boilerplate_array`
    (`JitBoilerplateArrayAllocationPlan`).
  - The template tier reuses `STUB_JIT_NEW_ARRAY` with an empty span; the
    VM reads the constant from the published pc.
- **Fixed latent GC-rooting bugs** in `Array.prototype.reverse`, `copyWithin`
  and `fill`: the receiver and values were held unrooted across `[[Set]]`.
  They are now handle-scoped.

**Results:**
- `[0..9]` literal: 168 B → 48 B (node 32).
- splay wall: 0.73 s → 0.59 s.
- Instructions: splay −12.5%.

**Open:**
- navier-stokes +4.2% and crypto +3.3% instructions vs ftdiv. These were
  measured at load 25–30; navier has no constant literals, and only tier-up
  timing differed. Re-measure at low load; fix if real.

**Gates:**
- Done: bytecode/compiler/gc/vm/jit lib tests, difftest 182/0 (new corpus
  `array_literal_boilerplate.js`), clippy on host and x86, x86 JIT lib tests.
- Pending: full test262 → `test262_results/full-run-2026-10-19i.log`. Then
  `python3 scripts/dev/t262_failset.py test262_results/latest.json benchmarks/results/parity-2026-10-19/t262-fail-base.txt`
  must report "failing 15 (base 15), new 0". If the run died, rerun it.

**Then commit** as
"perf(vm,jit,compiler): constant array literals share copy-on-write element storage".

## Next work, ordered by measured cost

1. **String allocation-site pretenuring (splay).**
   - Promotion census: strings 410 MB vs objects 16 MB vs arrays 6 MB
     promoted over a splay run. The cause is concat results
     `'String for key ' + k + ' in leaf node'` (cons ropes plus a number
     string, about 3 cells each).
   - Node stops scavenging after 36 ms; we do 14 scavenges at ~7.5 ms each.
   - Give Add / template-literal concat sites an allocation site (the
     `TypedFeedbackSlot::AllocationSite` path, `literal_site_placement`),
     tag young strings with it, and allocate decided-tenure sites old. This
     covers the interpreter, `STUB_STRING_CONCAT_ALLOC` and the graph
     `PrimitiveAdd`.
   - Extend the same mechanism to `new C()` receivers (NewReceiver) and
     closures.
2. **String cell compaction.**
   - `JsStringBody` is a fixed 56 B: map, atom, len, hash, a 28 B repr
     union and utf16_cache. Every cons/seq string pays that.
   - Port the size-per-variant layout: cons 32 B (header, map, len, hash,
     tag/depth, left, right), seq = 24 B plus characters in trailing
     storage, atom/cache only where needed.
   - Update the JIT string repr offsets (STRING_REPR_* consumers).
   - Hits RSS everywhere (ts, regexp, earley).
3. **Concurrent / incremental old-gen marking.**
   - Splay pays 3 STW mark-sweeps at 38 ms each; node pays 1.3 ms.
   - Blockers and the facts are in memory `gc_concurrent_marking_audit_2026_10_19.md`:
     - traced malloc side storage in generators, parked frames, promise
       reaction Vecs, ArrayExoticSlots HashMaps and ExoticSlots host_data;
     - a non-atomic `shade_gray` and VecDeque worklist;
     - scavenges during a mark are unhandled;
     - ArrayBody trace writes its cache.
   - Move traced storage into GC cells first, then concurrent marking with
     black allocation and concurrent sweep.
4. **Array shell 48 → 32 B.**
   - Drop `elements_ptr`, `dense_cap` and the duplicated kind/guard. Kind
     goes into a per-kind array map word, or the base is computed from
     cage + slab offset.
   - 40 B is reachable cheaply: drop `dense_cap`, read the length from the
     slab.
5. **regexp 3.29× node.** Per-exec GC is 13% (4 cells per exec); glue cost.
   JSC-style regexp JIT and match-result reuse.
6. **ts 2.82×.**
   - Megamorphic named access is 34% of optimized samples; the cached store
     hit path is 80–100 instructions.
   - Call-entry prologue is 8.8%.
7. **deltablue 2.41×.** Plan.execute loop is 154 vs 87 instructions per
   iteration; polymorphic calls.
8. **RSS and startup:** the persistent isolate snapshot (an in-process one
   exists; it needs serialization) toward bun's 7.6 ms.

## Hard rules (also in memory MEMORY.md — read it)

**Git and working tree**
- Work only in main: no branches, worktrees, stash, checkout or reset.
- Never touch foreign untracked files.
- Commit with
  `git -c user.name="Alexander Streltsov" -c user.email="funyloony@gmail.com" commit`.
  The GPG signature must show `G`.
- NO Co-Authored-By or AI trailer.
- Commit in a separate call after reading the test output.
- No "remaining" or "not done" text in commit messages.
- Keep files in the repo (scripts/dev, benchmarks/results/<slice>), never
  /tmp. Never write design-doc .md files.

**Engine design**
- No env flags or feature toggles.
- No `thread_local!`, `static mut` or process-global state.
- No Rc instead of stable addresses.
- No bridges or compat layers; change internal contracts in place.
- Preserve GC roots: multi-allocation natives use handle scopes, verified
  under `OTTER_GC_STRESS=1 OTTER_GC_VERIFY=1`.
- New or changed Rust modules get `//!` docs (purpose, # Contents,
  # Invariants, # See also).

**Tests and builds**
- Extend existing tests; add new ones only for new mechanisms.
- Never run `cargo test --workspace`; at most one or two `--test` targets.
- rustfmt leaf files only (`rustfmt --edition 2024 <file>`); lib.rs/mod.rs
  recurse, so use `--check` first.
- Disk: if under 15 GB free, `rm -rf target/*/incremental`; never use
  `cargo clean --target`.
- The CLI crate is `otter-cli`; the binary is `target/release/otter`.

## Gates before each JIT/GC/VM commit

**1. Lib tests**

```
cargo test -p otter-vm --lib; cargo test -p otter-jit --lib   (+ bytecode/compiler/gc when touched)
cargo run --release -q -p otter-difftest          # all passed, 0 failed
cargo clippy -p <crates> --all-targets -- -D warnings
cargo clippy -p <crates> --target x86_64-apple-darwin -- -D warnings
```

**2. Full test262** (background, timeout 7200000)

```
bash scripts/test262-full-run.sh > test262_results/full-run-<date><letter>.log
python3 scripts/dev/t262_failset.py test262_results/latest.json benchmarks/results/parity-2026-10-19/t262-fail-base.txt
```

**3. Commit and memory**
- Commit messages carry before/after numbers and the load average.
- Update memory `session_2026_10_19_engine_parity.md` (or a new session
  file) and the MEMORY.md index after each commit.

## Useful tools

- **Profiling:** `scripts/dev/launchprof.py <out> 1 -- <bin> <js>` (xctrace;
  use one run with `timeout 900`; BOUNDARY=1, FOCUS=<fn>).
- **Hot code:** `scripts/dev/loop_blockers.py`,
  `scripts/dev/outside_prof.py`, `scripts/dev/hot_nodes_ops.py`.
- **otter flags:** `--trace-gc`, `--jit-events=<path>`, `--interpreter`
  (semantic oracle), `--jitless`.
- **Footprint probe:** `scripts/dev/probe/object_footprint.js`
  (`--expose-gc`; compare with node).
- **COW semantics probe:** `scripts/dev/probe/cow_arrays.js`.
