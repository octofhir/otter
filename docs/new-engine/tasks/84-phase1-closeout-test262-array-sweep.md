# Task 84 — Phase 1 closeout: regression suite + test262 Array sweep

## Status

- [ ] cycle-reclamation regression suite consolidated
- [ ] cap-as-`RangeError` JS surface
- [ ] `bash scripts/test262-safe.sh built-ins/Array` runs to completion on a 16 GB host
- [ ] full `cargo test --workspace` green
- [ ] documentation updates
- [ ] gates green

## Goal

Prove the blocker is fixed. After this task: a developer can run the
full test262 Array sweep on their machine without the host getting
OOM-killed, and the `WeakMap` / `WeakSet` / `WeakRef` /
`FinalizationRegistry` semantics behave per-spec.

## Source

[`docs/new-engine/gc-architecture.md`](../gc-architecture.md) §1.2
NF3, §7.4 (repro harness), §8 Phase 1 exit criteria.

## Scope

1. **Consolidated regression suite.** Move the per-task regression
   tests from tasks 76–83 into one well-named file
   `crates-next/otter-vm/tests/gc_phase1_regressions.rs` so future
   refactors can run them as a unit. Tag each with a comment
   referencing its origin task.
2. **`RangeError` wrapping** — when the cap fires inside a script,
   surface `OtterError::OutOfMemory` to the embedder **and** make it
   catchable as `RangeError` from JS via the existing error-class
   machinery (mirror how spec stack-overflow surfaces today). Add a
   regression test:
   ```js
   try { let a = []; while (true) a.push(0); } catch (e) {
       assert(e instanceof RangeError);
   }
   ```
3. **Test262 sweep.** Run `bash scripts/test262-safe.sh built-ins/Array`
   end-to-end on a 16 GB dev host (the runner already exists in the
   legacy stack; replicate the safe-mode wrapper for the
   `crates-next/otter-test262` runner if not already present —
   architecture doc §1.2 NF3 commits to this gate). Capture before /
   after pass-rate numbers; document in
   `docs/new-engine/test262-baseline/`.
4. **Architecture-doc update.** In
   `docs/new-engine/gc-architecture.md` §8 Phase 1 exit criteria,
   tick the box and add the actual measured pass count + peak host
   RSS during the run.
5. **README index.** In
   [`docs/new-engine/tasks/README.md`](./README.md), record the
   completed Phase 1 in the same shape as Phases A–G ("✅ Phase
   complete — see Phase 2 master ticket"). Link to task 85 as the
   next scheduled work.
6. **Remove the long-standing `task 57` reference** in
   `lib.rs:194` if any survived (should already be gone after task
   80).

## Out of scope

- Phase 2+ work. Task 85 onward is its own track.
- Performance benchmarking. The Phase 1 deliverable is correctness
  and survivability, not speed. A `cargo bench` baseline before /
  after is welcome but not gating.

## Validation gates — production-grade bar (architecture doc §1.2)

### Functional correctness

- [ ] `bash scripts/test262-safe.sh built-ins/Array` reaches
  completion; pass rate documented in
  `docs/new-engine/test262-baseline/`.
- [ ] **Full test262 corpus** runs to completion across all
  built-ins/* and language/* (not just Array). Per-test heap cap
  honoured.
- [ ] `cargo test --workspace --all-features` green.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo fmt --all` clean.
- [ ] `cargo run -p otter-cli -- test --suite engine` green.
- [ ] No `Rc<RefCell<…>>` survives in `crates-next/otter-vm/src`
  inside any `Value`-variant body (`grep -rn "Rc<RefCell" crates-next/otter-vm/src`
  zero relevant hits; Shape / module URL strings exempted).

### Memory safety (NF7)

- [ ] `cargo +nightly miri test -p otter-gc` green.
- [ ] `cargo +nightly miri test -p otter-vm` green on the GC
  smoke-test set.
- [ ] AddressSanitizer build of the test262 runner (or a curated
  subset) green: `RUSTFLAGS="-Z sanitizer=address" cargo +nightly test -p otter-gc`.
- [ ] Every `unsafe` block in `crates-next/otter-gc/` has
  `// SAFETY:`; every public `unsafe fn` has `# Safety` in its
  docstring (PR-review checklist; CI-grep gate).

### Performance (NF1, NF2)

- [ ] STW old-gen pause histogram captured: 99p ≤ 200 ms at 1 GB
  live, ≤ 50 ms at 256 MB live.
- [ ] Young-gen scavenge histogram captured: 99p ≤ 5 ms at 4 MB
  nursery, ≤ 10 ms at 16 MB nursery.
- [ ] Allocation throughput micro-bench: ≤ 10 ns per young-gen
  alloc fast path; ≤ 30 ns including barrier'd pointer init.
  Captured via Criterion in `crates-next/otter-gc/benches/`.

### Endurance (NF8)

- [ ] **24 h soak test:** continuous test262 loop on a developer
  machine with no OOM, no panic, no RSS drift > 10 % from cycle 1.
  Documented in baseline directory with start/end RSS + GC stats
  histogram.
- [ ] **`cargo fuzz`** corpus on `GcHeap` public API: 10 M
  iterations no-panic, no leak (LeakSanitizer). At least three
  fuzz targets:
  `fuzz_alloc_collect_cycle`, `fuzz_handle_scope_nesting`,
  `fuzz_weakmap_eviction_pattern`.
- [ ] Long-running steady-state RSS (NF3): ≤ 1.5× live-set after
  6 h of continuous allocation/collection cycles.

### Diagnostic surface (NF6)

- [ ] `Runtime::heap_snapshot()` produces a valid snapshot.
- [ ] `Runtime::write_devtools_snapshot(path)` writes a
  `.heapsnapshot` file consumable by Chrome DevTools "Memory"
  panel. Manual smoke test documented.
- [ ] `Runtime::heap_stats()` exposes pause-time histogram,
  allocation rate, GC cycle count.

### Pointer-compression invariants (NF9)

- [ ] All heap pointers fit in the 4 GiB cage. Property tested
  (`proptest`) with random allocation sequences.
- [ ] Cage exhaustion surfaces `OutOfMemory` cleanly; no
  out-of-cage pointer ever leaks to the mutator.
- [ ] miri test on `Gc<T>` compress/decompress round-trip across
  scavenger object moves.

### Spec hygiene (per `tasks/README.md` §Working rules 6)

- [ ] Every module in `crates-next/otter-gc/` opens with the
  ADR-0001 §6 docstring.
- [ ] Every spec-implementing function (WeakRef, FinalizationRegistry,
  ephemerons) cites `https://tc39.es/ecma262/#sec-…` in its
  docstring.

## Closing

Gates from [`README.md`](./README.md#closing-a-task). Tick 84 in
[70-gc-master-tracker.md](./70-gc-master-tracker.md). At this point
all of Phase 1 is closed — leave the master tracker file alive, but
collapse tasks 71–84 entries into a single ✅ row and link the
post-mortem snapshot in `test262-baseline/`. Delete this task file.
