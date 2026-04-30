# Task 103 — Test262 outcomes + negative-test handling

**Parent:** [100 — Test262 conformance](./100-test262-conformance.md).
**Predecessor:** [102 — Harness + metadata](./102-test262-harness-and-metadata.md).
**Successor:** [104 — Baseline + diff](./104-test262-baseline-and-diff.md).

## Scope

Wire the per-test execution loop: take a parsed
`Frontmatter` + harness-preamble + test body, run them on a fresh
`OtterRuntime`, and produce an
[`Outcome`](./100-test262-conformance.md#outcome-taxonomy). Cover
every spec mapping rule from task 100 §"Outcome taxonomy" and
§"Negative-test handling". Wire the `--max-heap-bytes` cap, the
per-test wall-clock timeout, and a `catch_unwind` crash trap.

## Why

The runner has parsed metadata and a harness in slice 102 but
nothing actually runs yet. This is the heart of the conformance
machinery — without it the project still cannot quote a number.

## Deliverables

1. **`Outcome` enum** in `runner.rs`:

   ```rust
   pub enum Outcome {
       Pass,
       Fail { reason: String, stack: Option<String> },
       Skipped { feature: String },
       Crash { panic: String },
       Timeout { ms: u64 },
       OutOfMemory { bytes: u64 },
   }
   ```

   Plus a `TestResult { path, esid, frontmatter, outcome,
   wall_ms, peak_bytes }` record the writers in slice 104 will
   consume.
2. **`run_one(test_path) -> TestResult`** — driver that:
   1. Reads + parses the test source.
   2. Filters on `features` / `flags` per `feature_map.rs`. Early
      return as `Skipped` when needed.
   3. Builds a fresh `OtterRuntime` with the configured
      `--max-heap-bytes` cap.
   4. Compiles + runs the harness (cached per-worker).
   5. Compiles + runs the test body. Routes through
      `OtterRuntime::run_module_async` when `flags: [module]`,
      otherwise `run_script`. Async tests wait on the `$DONE`
      polyfill from slice 102.
   6. Maps the result to an `Outcome` per the rules below.
3. **Negative-test inversion** per task 100 §"Negative-test
   handling":
   - `phase: parse` matches when `CompileError::Syntax(...)`
     fires *before* run start.
   - `phase: resolution` matches when the linker rejects.
   - `phase: runtime` matches when `VmError::Uncaught { value }`
     fires AND `value` is an Error instance whose `.name` matches
     `negative.type`. The runner reads the throwable's `name`
     own property — the §41 closeout shipped the canonical
     names through `error_classes::ErrorKind::class_name`, so
     this is a direct string compare.
   - Any other phase / type pairing is `Fail { reason: "negative
     mismatch: expected X.{type} in {phase}, observed ..." }`.
4. **Heap cap** — each `OtterRuntime` is built with the runner's
   `--max-heap-bytes` (default 512 MB; env override
   `OTTER_TEST262_HEAP_BYTES`). On cap hit, the engine emits a
   catchable `RangeError("out of memory: heap limit exceeded")`
   per the existing `MemoryManager` plumbing — the runner
   inspects the runtime's reported peak and records
   `OutOfMemory { bytes }`.
5. **Wall-clock timeout** — wrap the per-test body run in a
   timer (default 30 s; env override
   `OTTER_TEST262_TIMEOUT_MS`). Pick a watchdog thread that
   sets `Interpreter::interrupt_handle().interrupt()` on fire;
   when the engine returns `VmError::Interrupted`, surface as
   `Timeout { ms }`.
6. **Crash trap** — wrap the entire `run_one` body in
   `std::panic::catch_unwind`. A panic inside the engine
   surfaces as `Crash { panic: <string-payload> }` and the
   runner moves on. Engineering note: panics are bugs — the
   diff report (slice 104) treats `Crash` as an automatic
   regression even when the previous baseline also crashed.
7. **Strict-mode policy**: foundation is always strict. Tests
   with `flags: [onlyStrict]` run normally; tests with
   `flags: [noStrict]` (only) record
   `Skipped { feature: "foundation-always-strict" }`; tests
   without strict-mode flags run once.
8. **Curated bring-up subset** — a `tests/curated.rs` integration
   test running ~100 hand-picked Test262 files covering every
   `Outcome` variant. Every variant must have at least one
   curated test; the integration test asserts the expected
   outcome distribution. CI uses this to detect regressions in
   the runner itself before it touches the full corpus.

## Files to touch

- `crates-next/otter-test262/src/runner.rs`
- `crates-next/otter-test262/src/isolation.rs` (fresh-runtime
  factory)
- `crates-next/otter-test262/Cargo.toml` (no new deps expected;
  `tokio` already present from 101 if that was the choice)
- `crates-next/otter-test262/tests/curated.rs`
- `crates-next/otter-test262/tests/curated/` (curated subset)

## Sequencing notes

- 102 must land first. This slice imports `Frontmatter`,
  `feature_map`, and the harness compiler.
- The `$DONE` polyfill stub from 102 gets its real implementation
  here — async resolution feeds the outcome mapper.
- Slice 104 imports `TestResult` from this slice.

## Gates

- `cargo test -p otter-test262 curated` passes.
- `cargo run -p otter-test262 -- run --filter
  'test/built-ins/Math/abs/**'` produces a sensible
  pass/fail/skip distribution (sanity-check by hand against
  the corpus contents).
- The curated test exercises each `Outcome` variant at least
  once.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- A pathological test (e.g.
  `test/built-ins/Array/length/define-own-prop-length-overflow.js`)
  records `OutOfMemory` instead of crashing the runner.

## Spec links

- Test262 negative semantics:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md#negative>
- ECMA-262 §16.1.5 ParseScript / §16.2.1.6 ParseModule (parse vs
  resolution distinction): <https://tc39.es/ecma262/#sec-parsescript>
- §10.4.3.1 InterruptCheck (the timeout mechanism rides
  `InterruptFlag` already): <https://tc39.es/ecma262/>
- ADR-0001:
  [`docs/new-engine/adr/0001-design-discipline.md`](../adr/0001-design-discipline.md)
