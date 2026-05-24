# Next Session Bootstrap

Copy-paste this prompt to start a fresh agent session against the
current main of `octofhir/otter`. Captures everything still open
after the 2026-05 architecture refactor (Phases 0–6) shipped.

---

## Repo state recap (as of 2026-05-24)

Architecture refactor 2026-05 fully closed. Phases 0, 1, 2.1–2.5,
3.1–3.3, 4.1–4.3, 5.1–5.3, 6.1 (deferred), 6.2, 6.3 — all done.
Plan + decision records deleted from `docs/`; their reasoning lives
in commit history.

Source surfaces shipped in the last session:

- `crates/otter-vm/src/inspect.rs` — `StepTracer`, IC / shape /
  frame snapshots, `HeapSnapshotSummary`, Chrome `.heapsnapshot`
  writer. Public via `otter_runtime::inspect`.
- `crates/otter-vm/src/groom.rs` + `otter_gc::SafeFinalize` —
  sweep-time finalize hook, opt-in per body.
- `crates/otter-macros/src/derive_groom.rs` —
  `#[derive(Groom)]` + `#[groom(skip)]` / `#[groom(via = …)]`.

Quality gate that every shipped commit cleared:

```
cargo clippy --all-targets --all-features -- -D warnings
cargo test --workspace --lib --tests
cargo run --release -q -p otter-test262 -- run --filter <…>
```

## House rules (hard)

1. **Public docs name no other JS engines.** Do not write
   "Boa-parity", "JSC-style", "V8 equivalent", etc. in any file
   under `docs/`, public mdBook content, README, or
   crate-level rustdoc that surfaces in publish output. Phrase
   features in self-contained terms. Internal-only artifacts
   (private memos, commit messages, MEMORY notes) may still cite
   inspiration sources.
2. **No competitor engine names in inline source rustdoc either**
   if that crate publishes docs. The macros crate publishes — its
   rustdoc has been scrubbed; keep it that way.
3. **No backward-compat shims.** Otter is pre-1.0. Renames are
   hard cut-overs on short-lived branches.
4. **Every commit body block ends with:**
   `Co-Authored-By: Claude Opus 4.7 (1M context) <noreply@anthropic.com>`
5. **Test262 baseline must not regress on any commit.** Spot
   check the family you touch.

## Open work — pick what fits the session

### Tier 1 — high-value test262 wins

1. **Proxy: `Function.prototype.apply` compiler support.**
   `built-ins/Proxy` is 219/311; the remaining ~95 fail on the
   compiler-side `FEATURE_NOT_IN_SLICE` for `target.apply(thisArg,
   args)` in test bodies. Lands in the compiler / call-dispatch
   side, not in `crates/otter-vm/src/proxy.rs`. Acceptance:
   `built-ins/Proxy` ≥ 290/311 with no regression elsewhere.

2. **Temporal `[[Construct]]` argument shapes.**
   `built-ins/Temporal` at 208/4603. Direct
   `new Temporal.Instant(epochNs)` and partial-record ctor shapes
   throw `TypeError`. Wiring at
   `crates/otter-vm/src/temporal/intrinsic.rs::temporal_class_direct_construct`.
   Bulk of remaining 4393 fails depend on `[[Construct]]` +
   prototype chains + proposal-temporal property surface. Even a
   partial fix here is worth thousands of test262 wins.

3. **Object 15 residual failures.** `built-ins/Object` 3391/3414.
   Remaining 15 are isolated spec gaps — pick off one at a time:
   - `seal/seal-asyncfunction.js` family — async-function class
     wrapping.
   - `Object.prototype.toString` for Proxy-of-function.
   - `nan-equivalence` redefine corners.
   - `Object.getOwnPropertyNames(15.2.3.4-4-2)` for prototype
     overrides.

### Tier 2 — VM correctness / robustness

4. **Compiler operand cap audit.** Dense `NewArray` capped at 240
   elements (commit 52c82d31). The same `u8::MAX` ceiling
   affects `MakeClosure`, template raw / cooked list, call-arg
   windows. Audit every variadic opcode + add chunked-fallback
   where the cap can be hit by user input. Add fuzzer-style tests
   that build literals at the boundary.

5. **`is_object_like` audit follow-up.** Commit b1d67278 fixed 5
   spec-Object widening sites. `value/mod.rs` still has internal
   accessor-extractor sites that intentionally use the narrow
   form. Re-grep on every new builtin and keep
   `is_object_like` / `is_object_type` honest. No standalone task
   — fold the check into PR-time review.

### Tier 3 — DX / migration

6. **Per-body `Pelt` / `Groom` migration.** Phase 6.3 shipped the
   derive infrastructure. Remaining hand-written `SafeTraceable`
   impls to migrate (one body per commit, smallest first):

   - `JsRegExpBody` — `crates/otter-vm/src/regexp.rs`
   - `BoundFunctionBody` — `crates/otter-vm/src/bound_function.rs`
   - `NativeFunctionBody` — `crates/otter-vm/src/native_function.rs`
   - `ArrayBody` — `crates/otter-vm/src/array.rs`
   - Two generator bodies — `crates/otter-vm/src/generator.rs`
   - Four collection bodies (Map/Set/WeakMap/WeakSet) —
     `crates/otter-vm/src/collections.rs`
   - Weak-ref bodies — `crates/otter-vm/src/weak_refs.rs`
   - `PurePromiseBody` — already on `#[derive(Pelt)]`; revisit
     when `Groom` is needed for cross-thread settlement bookkeeping.
   - Four iterator-state bodies — `crates/otter-vm/src/iterator_state.rs`
     (currently an `enum`, blocks `Pelt`; needs enum support in the
     derive or hand-written stay).

   Migration template:
   1. Read the existing hand-rolled `SafeTraceable` impl.
   2. Convert struct + add `#[derive(Pelt)]` with `#[pelt(tag =
      …)]`.
   3. Tag every non-GC primitive `#[pelt(skip)]`.
   4. If the body needs sweep-time cleanup beyond `Drop`, add
      `#[derive(Groom)]` + register via
      `heap.register_finalize::<MyBody>()` at bootstrap.
   5. Run `cargo test -p otter-vm --lib`, full test262 spot
      check.

### Tier 4 — performance follow-ups

7. **Object internal-method vtable (deferred from 6.1).**
   Re-trigger conditions (any one is enough):
   - `PropertyIcStats::load_hits / (load_hits + load_misses)`
     drops below 80 % on a representative workload.
   - Average chain branch depth above 4 (gate behind a
     `vm-dispatch-counters` feature flag, then measure).
   - Measurable Proxy-heavy benchmark regression.

   Reasoning + measurement details are in commit 911487e5 (see
   `git show 911487e5`).

## Operational helpers

- Step trace from CLI:
  `otter --trace=- run script.ts` (or `--trace=path.log`).
- Heap snapshot from runtime:
  `runtime.write_chrome_heap_snapshot(&mut writer)` →
  Chrome DevTools `.heapsnapshot`.
- Step-trace goldens regenerate via
  `OTTER_BLESS_TRACES=1 cargo test -p otter-runtime --test step_trace_golden`.
- Test262 baseline runner:
  `cargo run --release -q -p otter-test262 -- run --filter <pattern> --timeout 5000`.

## When in doubt

- Re-read `AGENTS.md` for the active-stack contract.
- Re-read `CLAUDE.md` for the per-repo rules.
- Re-read `ROADMAP.md` for the long-arc roadmap (architecture
  refactor was a sub-plan; the broader product roadmap lives
  there).
- mdBook chapters worth knowing: `engine/architecture.md`,
  `engine/native-call-abi.md`, `engine/step-trace.md`,
  `engine/gc-api.md`, `macros/overview.md`, `macros/design.md`.
- For the GC API surface: `crates/otter-gc/src/lib.rs` re-exports
  `SafeFinalize`, `SafeTraceable`, `Traceable`,
  `GcHeap::register_finalize`.

Pick a task. State the acceptance criterion up front. Ship in one
commit on `main`. Gate with clippy + workspace tests + a targeted
test262 spot check.
