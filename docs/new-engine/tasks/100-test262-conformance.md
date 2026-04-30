# Task 100 — Test262 conformance runner

**Status:** planning. Implementation lives in tasks
[101](./101-test262-runner-skeleton.md) / [102](./102-test262-harness-and-metadata.md)
/ [103](./103-test262-outcomes-and-negative.md) / [104](./104-test262-baseline-and-diff.md)
/ [105](./105-test262-ci-integration.md). Do not start coding before
the slice files are read in order.

## Goal

Publish a versioned ECMA-262 conformance baseline for the new-engine
stack (`crates-next/*`) by driving the official
[`tc39/test262`](https://github.com/tc39/test262) corpus through a
fresh `crates-next/otter-test262` runner. Every PR posts a diff
against the previous baseline; regressions block merge. The number
to beat: V8 / SpiderMonkey publish their pass rates against the
same corpus, so the runner gives the project a comparable surface.

The §41 spec-gap audit (rows 41–82, including the closeout sweep)
shipped a meaningful Test262 surface — without a runner there is no
measurable parity claim. This task closes that loop.

## Why now

- Spec gaps are closed (see `41-spec-gap-audit.md` §Status), so a
  conformance run will return real numbers instead of just timing
  out on missing features.
- The legacy `crates/otter-test262` runner under the parked tree is
  reference-only — it speaks the old VM ABI. We need a fresh runner
  on the active stack.
- V8 / SpiderMonkey / JavaScriptCore each publish a Test262 status
  matrix; OtterJS needs the same to make claims about parity.

## Scope

In:

- New `crates-next/otter-test262` workspace member.
- `vendor/test262` git submodule pinned to a known commit.
- Frontmatter parser (YAML between `/*--- ... ---*/`) covering
  every field listed in
  [INTERPRETING.md](https://github.com/tc39/test262/blob/main/INTERPRETING.md).
- Per-test isolation: fresh `OtterRuntime` per file, harness
  preamble (`assert.js`, `sta.js`, then per-test `includes`)
  compiled once per worker and replayed via the existing
  [eval hook](../../../crates-next/otter-runtime/src/lib.rs).
- Outcome taxonomy: `Pass` / `Fail(reason)` / `Skipped(feature)`
  / `Crash(panic)` / `Timeout(ms)` / `OutOfMemory(bytes)`.
- Negative-test handling per
  [§INTERPRETING — Negative](https://github.com/tc39/test262/blob/main/INTERPRETING.md#negative).
- JSON + Markdown reports under
  `docs/new-engine/test262-baseline/<commit>.{json,md}`,
  sliced by spec section.
- `--diff <previous-baseline>` mode reporting regressions /
  improvements per file.
- CLI surface: `cargo run -p otter-test262 -- run [flags]` plus
  `just test262` in the workspace `justfile`.
- Sharding (`--shard N/M`) so CI can parallelise.
- GitHub Actions integration: PR-comment publishing, regression
  gate on every push.

Out:

- Live tracking against test262's `staging` branch (only the
  pinned `main` commit).
- Coverage-instrumented runs.
- Exhaustive feature-flag → spec-section maps; the runner reports
  `Skipped(<feature-name>)` and a follow-up task curates the
  feature list.
- `wait` / `notify` / `waitAsync` semantics under threads
  (single-thread foundation already correct per row 76).

## Source acquisition

```sh
# One-time
git submodule add https://github.com/tc39/test262.git vendor/test262
git submodule update --init --remote vendor/test262
cd vendor/test262 && git checkout <pinned-commit>
cd ../..
git add vendor/test262
```

The pinned commit lands in `.gitmodules` as `branch = main` plus a
recorded SHA. The runner refuses to start if `vendor/test262` is
absent or unpinned. Bumping the pin is a deliberate PR — never
implicit.

## Crate skeleton (target shape — implemented in slice 101)

```
crates-next/otter-test262/
├── Cargo.toml
├── README.md
└── src/
    ├── lib.rs              — re-exports
    ├── main.rs             — CLI entry (clap)
    ├── harness.rs          — assert.js / sta.js / includes loader
    ├── metadata.rs         — frontmatter parser (serde_yaml)
    ├── runner.rs           — per-test driver + outcome enum
    ├── isolation.rs        — fresh `OtterRuntime` per test
    ├── shard.rs            — `--shard N/M` walking
    ├── report.rs           — JSON + Markdown writers
    ├── diff.rs             — baseline diff
    └── feature_map.rs      — Test262 `features:` → engine-readiness
```

Dependencies (no others without RFC):

- `otter-runtime`, `otter-compiler`, `otter-bytecode` (workspace).
- `walkdir`, `ignore` for traversal.
- `serde`, `serde_yaml`, `serde_json` for frontmatter / reports.
- `rayon` or `tokio::task::spawn_blocking` for parallel sharding.
- `indicatif` for progress bars.
- `anyhow`, `thiserror` for errors.
- `clap` for CLI.

## Test262 frontmatter

Every test starts with:

```js
/*---
description: |
  ...
esid: sec-...
info: |
  ...
features: [BigInt, Atomics]
flags: [module]
includes: [propertyHelper.js]
negative:
  phase: parse
  type: SyntaxError
---*/
```

- **`description`**: ignored at runtime, surfaced in failure reports.
- **`esid`**: spec section anchor — the report groups by this when
  available, otherwise by directory path.
- **`info`**: ignored.
- **`features`**: filter. If any listed feature is in the
  engine-not-yet-ready set, mark as `Skipped(feature)`. The
  feature → readiness map lives in `feature_map.rs` and is the
  single source of truth — adding a shipped feature flips its
  bucket.
- **`flags`**: union of `[onlyStrict, noStrict, module, raw, async,
  generated, CanBlockIsFalse, CanBlockIsTrue, non-deterministic]`.
  Foundation-relevant subset:
  - `module` — wrap the test as an ES module entry; harness still
    applies. Default is `script`.
  - `raw` — skip the harness preamble entirely.
  - `async` — append `$DONE` polyfill; pass iff `$DONE()` fires
    without an argument; fail iff called with a value.
  - `noStrict` — foundation is always strict; record as
    `Skipped("noStrict-only")` per ADR-0001.
  - `onlyStrict` — already covered.
  - `CanBlockIsFalse` — skip; we are CanBlockIsFalse.
- **`includes`**: list of harness fragments under
  `vendor/test262/harness/`. Loaded in order before the test body.
- **`negative`**: `{ phase, type }` — see "Negative-test handling"
  below.

Spec link:
<https://github.com/tc39/test262/blob/main/INTERPRETING.md#metadata>

## Test execution model

Per file:

1. Parse frontmatter.
2. Filter on `features` / `flags` against the readiness map.
3. Build the harness preamble: `assert.js` + `sta.js` + every
   `includes` entry, concatenated in the listed order.
4. Allocate a fresh `OtterRuntime` (per-test isolation — no shared
   globals between tests).
5. Compile + run the harness via `OtterRuntime::run_script`
   (already wired through the eval hook).
6. Compile + run the test body. `flags: [module]` routes through
   `OtterRuntime::run_module_async`; otherwise `run_script`.
7. Map the outcome.

The fresh-runtime contract matters: tests routinely poison globals
(`assert = function() { fail() }` to verify negative paths) and we
must not let one test's mutation leak into the next.

## Outcome taxonomy

```rust
pub enum Outcome {
    Pass,
    Fail { reason: String, stack: Option<String> },
    Skipped { feature: &'static str },
    Crash { panic: String },
    Timeout { ms: u64 },
    OutOfMemory { bytes: u64 },
}
```

Mapping rules:

- Test body returns normally + frontmatter has no `negative` →
  `Pass`.
- Test body throws + frontmatter has `negative: { phase, type }`
  matching → `Pass`.
- Test body throws when no `negative` → `Fail`.
- Test body returns when `negative` was set → `Fail`.
- Compile-time `CompileError` fires before runtime → `negative.phase
  = parse` matches; otherwise `Fail`.
- Linker rejection (module link error) → `phase = resolution`.
- `VmError::Uncaught { value }` → `phase = runtime`.
- Wall-clock exceeds the per-test timeout (default 30 s) →
  `Timeout`.
- Per-test heap cap (default 512 MB) hit → `OutOfMemory`. The cap
  surfaces inside the engine as a catchable `RangeError("out of
  memory: heap limit exceeded")` per the existing
  `--max-heap-bytes` infrastructure. The runner records the
  observed-bytes-at-cap.
- Rust panic inside the engine → `Crash`. `std::panic::catch_unwind`
  wraps the per-test driver so one crash never derails the suite.

## Negative-test handling

```yaml
negative:
  phase: parse | resolution | runtime
  type: SyntaxError | TypeError | ReferenceError | RangeError | URIError
```

Mapping table:

| Phase | When error fires | Engine-side path |
|---|---|---|
| `parse` | Before any code runs | `CompileError::*` from `otter-compiler` |
| `resolution` | At module link time | linker error from `otter-runtime::ModuleLoader` |
| `runtime` | During execution | `VmError::Uncaught { value }` whose `value` is an Error instance with matching `name` |

Pass criterion: phase matches AND `type` matches the thrown
error's `[[Class]]` (or the compile-error class for `parse` —
`CompileError::Syntax` ≡ `SyntaxError`, etc.). Anything else is a
fail.

Spec:
<https://github.com/tc39/test262/blob/main/INTERPRETING.md#negative>

## Output formats

### `docs/new-engine/test262-baseline/<commit>.json`

```json
{
  "test262_commit": "<sha>",
  "engine_commit": "<sha>",
  "ran_at": "2026-05-01T12:00:00Z",
  "totals": {
    "total": 51234,
    "passed": 41123,
    "failed": 8123,
    "skipped": 1832,
    "crashed": 0,
    "timed_out": 7,
    "oom": 149
  },
  "by_section": {
    "language/expressions": { "total": 7234, "passed": 6800, "failed": 434 },
    "...": {}
  },
  "failing_tests": [
    {
      "path": "test/built-ins/Array/prototype/...",
      "outcome": "Fail",
      "reason": "TypeError: ...",
      "esid": "sec-array.prototype.flat"
    }
  ]
}
```

### `docs/new-engine/test262-baseline/<commit>.md`

Human-readable summary table sliced by directory + section, plus
the top 100 failing tests by recurrence pattern. Both files commit
together so blame on the JSON shows the engine commit and the
test262 commit that produced it.

### `--diff <previous>` mode

```text
test262 diff against docs/new-engine/test262-baseline/abc1234.json:
  +147 newly passing
   -3 regressed:
     - test/.../foo.js  (was Pass, now Fail: TypeError)
     - test/.../bar.js  (was Pass, now Timeout(30000))
     - ...
   ±0 unchanged: 49981
```

The CI gate fails on any non-zero regression count.

## CLI surface (slice 101 / 104)

```sh
# Full run
cargo run -p otter-test262 -- run

# Filter to a glob
cargo run -p otter-test262 -- run --filter 'test/built-ins/Array/**'

# Sharding (CI: spawn 8 jobs, each with --shard 1/8 .. 8/8)
cargo run -p otter-test262 -- run --shard 3/8

# Per-test guards
cargo run -p otter-test262 -- run --timeout 30000 --max-heap-bytes 536870912

# Diff
cargo run -p otter-test262 -- diff docs/new-engine/test262-baseline/abc1234.json
```

`just test262` runs the full sweep with the canonical safety
caps. `just test262-filter "Array"` is the curated filter form.

## Safety controls

Two layers, both required for an unattended full run:

1. **Inner cap (engine heap limit).** Threaded as
   `--max-heap-bytes 512MB` per test; surfaces as a catchable
   `RangeError`. Already in via the foundation work referenced in
   `MEMORY.md` (per-runtime `MemoryManager` with the fresh
   `MemoryManager::set_thread_default` plumbing).
2. **Outer cap (OS ulimit).** `scripts/test262-safe.sh` wraps the
   runner in `ulimit -v 4G` on Linux to backstop allocations the
   engine doesn't see (e.g. native-side `Vec` growth in built-ins).
   This is the only sanctioned way to run pathological suites.

Three operator-rules ported from `MEMORY.md`:

- **Never** run multiple test262 runners in parallel — they share
  the host memory budget and OOM the box.
- **Never** run with timeouts longer than 30 s per test unless the
  user explicitly asks (per `feedback_no_long_test262.md`).
- The runner refuses to launch on debug builds without
  `--allow-debug` (debug-mode runs take 30+ minutes and are not the
  workflow we want by default).

## CI integration (slice 105)

GitHub Actions:

```yaml
name: test262
on:
  pull_request:
  push:
    branches: [main]

jobs:
  test262:
    runs-on: ubuntu-latest-32-core
    strategy:
      matrix:
        shard: [1, 2, 3, 4, 5, 6, 7, 8]
    steps:
      - uses: actions/checkout@v4
        with: { submodules: recursive }
      - uses: dtolnay/rust-toolchain@stable
      - run: cargo build --release -p otter-test262
      - run: |
          bash scripts/test262-safe.sh \
            --shard ${{ matrix.shard }}/8 \
            --output reports/shard-${{ matrix.shard }}.json
      - uses: actions/upload-artifact@v4
        with:
          name: test262-shard-${{ matrix.shard }}
          path: reports/

  aggregate:
    needs: test262
    runs-on: ubuntu-latest
    steps:
      - run: cargo run -p otter-test262 --release -- merge reports/*.json
      - run: cargo run -p otter-test262 --release -- diff main-baseline.json
      - uses: actions/github-script@v7
        with:
          script: |
            // Post diff as PR comment
```

The pinned `vendor/test262` commit only changes via a deliberate
PR. The CI gate fails on any regression; improvements record a new
baseline once `main` lands.

## Rollout plan

Five slices, sequenced. Each is a separate task file; do not skip
ahead.

1. **[101 — Runner skeleton](./101-test262-runner-skeleton.md)** —
   crate, `Cargo.toml`, submodule wiring, walkdir traversal that
   counts `.js` files under `test/`. Exit criterion: `cargo run -p
   otter-test262 -- run --dry-run` walks the corpus and reports a
   total count without running anything.
2. **[102 — Harness + metadata](./102-test262-harness-and-metadata.md)** —
   YAML frontmatter parser, `assert.js`/`sta.js`/`includes` loader,
   harness compilation cached per worker. Exit criterion:
   harness round-trips through the engine and the parser handles
   every frontmatter shape in the corpus without panicking.
3. **[103 — Outcomes + negative](./103-test262-outcomes-and-negative.md)** —
   `Outcome` enum, negative-test inversion, heap cap + timeout
   plumbing, `catch_unwind` crash trap. Exit criterion: a curated
   100-test subset reports the right outcomes (verified by hand).
4. **[104 — Baseline + diff](./104-test262-baseline-and-diff.md)** —
   JSON + Markdown writers, `docs/new-engine/test262-baseline/`
   layout, `--diff` subcommand, sharding + merge support. Exit
   criterion: first full run lands as
   `docs/new-engine/test262-baseline/<commit>.{json,md}`.
5. **[105 — CI integration](./105-test262-ci-integration.md)** —
   GitHub Actions workflow, PR-comment publisher, regression gate.
   Exit criterion: PR opens with a diff comment and rejects on
   regression.

## Dependencies

- The eval hook on `OtterRuntime` (already shipped) handles
  per-test compilation. No new opcodes required.
- `MemoryManager::set_thread_default` (already shipped) enforces
  the per-test heap cap.
- `OtterRuntime::run_module_async` (shipped under §M7) is the
  entry point for `flags: [module]` tests.
- The §41 audit closeout (every row 41–82) means the runner has a
  meaningful surface to grade against — without it, every test
  involving a generator / Proxy / TypedArray / Intl would skip.

## Out-of-scope follow-ups

File these as separate tasks once 101–105 land:

- **Test262 staging tracking.** Run a nightly job against the
  `tc39/test262@staging` branch to surface upcoming-spec breaks.
- **Coverage-instrumented runs.** `cargo llvm-cov` integration so
  test262 coverage feeds the engine's coverage badge.
- **Per-feature spec-section maps.** Today the runner records
  `Skipped(feature)` with the raw `features:` string; a curated
  table maps each feature to the responsible §-section so
  regression hunters can pivot quickly.
- **Spider/HermesJS comparators.** Side-by-side baseline against
  V8 / SpiderMonkey / Hermes for marketing parity claims.

## Spec links

- ECMA-262: <https://tc39.es/ecma262/>
- Test262 INTERPRETING.md:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
- Test262 CONTRIBUTING.md:
  <https://github.com/tc39/test262/blob/main/CONTRIBUTING.md>
- ADR-0001 (spec-link rule):
  [`docs/new-engine/adr/0001-design-discipline.md`](../adr/0001-design-discipline.md)

## Gates before declaring task 100 done

This is a planning task, not an implementation task. Done means:

- `100-test262-conformance.md` (this file) lands.
- `101` … `105` task files exist with the structure described
  under "Rollout plan".
- `41-spec-gap-audit.md` row 83 added under a new
  `## §83 — Conformance` section (not struck through — the row
  closes only when slice 105 lands).
- `tasks/README.md` indexes the five slices under a new
  `## Test262 conformance` section.
- `git status` shows only six new + two edited Markdown files.
  Zero changes under `crates-next/*` or `tests/`.

## Status

- Plan landed 2026-05-01.
- Implementation pending — start with task 101.
