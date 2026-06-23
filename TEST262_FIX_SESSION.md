# Mission: drive test262 conformance to 100% (ECMA-262 + ECMA-402)

Close every **fixable** test262 failure before starting tier2 (cranelift). Work
on branch `perf/tier1-close` (or a fresh `conformance/*` branch off it).

## Where things stand

- Gates GREEN: `cargo fmt --all --check` and
  `cargo clippy --all-targets --all-features -- -D warnings` both pass — keep
  them green every commit.
- Perf tier1 is wrapped (regex exec 37x, array push/pop, ExecutionContext
  borrow, json array — see memory `bun_gap_levers`). Conformance is the new
  focus.
- **SpiderMonkey staging is already config-ignored** (`b160cc7e`):
  `test262_config.toml` `ignored_tests` now has `"staging/sm"` (the whole
  subtree — SM-only harness/extensions, not ECMA). ~200 tests now `skip`.

## First action — get fresh authoritative numbers

The committed `ES_CONFORMANCE.md` is STALE (2026-06-12, old engine commit). Run a
fresh full pass FIRST, then categorize:

```
bash scripts/test262-full-run.sh          # crash-safe per-dir batches, ~15-30 min
#   merged JSON -> test262_results/latest.json
cargo run -p otter-test262 -- conformance test262_results/latest.json --output ES_CONFORMANCE.md
```
NOTE: the `built-ins/Atomics` batch is slow (`waitAsync` timeouts) and can stall
the full run — run it isolated with a short `TIMEOUT=5` or `EXCLUDE_DIRS` it and
do Atomics via `--filter` separately.

Extract the failing set per section:
```
cargo run --release -p otter-test262 --bin otter-test262 -- run --filter "<dir>" --output /tmp/x.json
python3 -c "import json;[print(t['path'],'::',t.get('reason','')[:80]) for t in json.load(open('/tmp/x.json'))['failing_tests']]"
```

## Failure landscape (from stale report, ~1753; ~1550 after the SM ignore)

Fix in this order — contained engine bugs first, the i18n subsystem last.

### Tier A — concrete engine bugs (DO FIRST, high ROI, low risk)
- **Atomics validation order (~50):** `validate-arraytype-before-{index,value,
  expectedValue,replacementValue}-coercion`, `non-shared-int-views-throws`.
  Fix = check the typed-array type / shared-ness and throw TypeError BEFORE
  coercing index/value args. One module (`crates/otter-vm/src/binary/` Atomics).
  (Note: default config skips `Atomics`/`SharedArrayBuffer` features in the
  unit-test toml but the real run grades them — confirm they're graded.)
- **Core language/built-ins corners (small per section, add up):**
  `built-ins/AsyncFunction/AsyncFunction-is-extensible`, `language/expressions/
  call` (8), `language/statements/class` + `expressions/class` corners (~20),
  `computed-property-names/class` (7), `language/expressions/call`. Group by exact
  `Test262Error` reason, fix per cluster.

### Tier B — Temporal completeness (~250, mostly 95-98% pass already)
`built-ins/Temporal/{ZonedDateTime 56, Duration 32, Instant 11, PlainDate 10,
PlainYearMonth 9, PlainTime 8, PlainDateTime 12, PlainMonthDay 7}` + the
`intl402/Temporal/*` mirror. Mostly edge cases + timezone-DB methods. Backed by
the `temporal_rs` crate — some failures are upstream bugs (already a few in
`ignored_tests` with that note); distinguish "our glue bug" vs "temporal_rs
bug" (the latter → ignore with an upstream-link comment, don't fake-fix).
Memory `computed_member_spread_call_receiver` + Temporal notes have prior work.

### Tier C — staging (non-sm) — triage
After `staging/sm` is gone, remaining `staging/*` = Stage-3 proposals being
staged (e.g. explicit-resource-management already feature-skipped). Triage: real
+ shipped → fix; not-yet-ECMA proposal → add the feature to `skip_features` (NOT
path ignore) so it's tracked as a proposal gap, not a failure.

### Tier D — Intl / ECMA-402 (~700-900) — **a SUBSYSTEM, decide explicitly**
`intl402/*`: DateTimeFormat (150), NumberFormat (119), Locale (91, **0%**),
DurationFormat (81+8, **0%**), getCanonicalLocales (38, **0%**),
supportedValuesOf (25, **0%**), ListFormat (57), Segmenter (49),
RelativeTimeFormat (53), Collator (17), PluralRules (14), DisplayNames (7),
intl402/Temporal (~200), intl String/BigInt. These need **ICU-grade locale data
+ algorithms** — realistically `icu4x` (+ bundled CLDR). This is its own
milestone, comparable in size to tier2; it is NOT a quick cleanup.
**Decision required:** either commit to an `icu4x`-backed Intl milestone, or
scope Intl as deliberately-partial (document it, mark the 0% subdirs) and treat
"100% conformance" as "100% of non-Intl". Current Intl lives in
`crates/otter-vm/src/intl/` (memory `intl_locale_impl` has prior partial work:
Intl.Locale 147/152).

## Workflow per fix (NON-NEGOTIABLE)

1. Pick a cluster (one section / one failure reason). Read the actual test files
   in `vendor/test262/test/...` — understand the exact spec step they assert.
2. Fix at the engine/runtime level (AST via `oxc`, never regex-parse JS).
3. **Differential gate:** capture failing set, stash, rebuild baseline, re-run the
   filter, diff failing sets. Require **0 NEW failures** in that section AND
   spot-check related sections. Property-escape timeouts are flaky (machine load)
   — ignore those.
4. `cargo test -p otter-vm --release` (+ touched crates).
5. `OTTER_GC_STRESS=64` on anything touching alloc/GC paths.
6. Keep `fmt --all --check` + `clippy -- -D warnings` GREEN.
7. Commit per cluster, conventional message, **NO Co-Authored-By trailer**.

## Hard rules (from memory — violating these made the user escalate)

- **NO Co-Authored-By / AI trailer** on any commit.
- **NO feature flags / env kill-switches / `OTTER_*` toggles** — single default
  path; revert via git if bad.
- **NO `thread_local!` / `static mut` / process-global** — per-instance state on
  GC body (`#[pelt(skip)]`), per-isolate on `Interpreter`.
- **NO simplified algorithms** — production-grade only; incremental coverage OK,
  simplified logic forbidden.
- **NO "Phase X"/task-number/"Tick" labels in code comments** — timeless
  behavior+why only.
- **Fix a module to 100% before moving on** (no bit-by-bit); never ship a
  regression (verify failing-SETS, not just counts).
- **Commit correct gated code, don't stash/hide it.**
- **Profile before optimizing** (samply + `benchmarks/profiles/symbolicate.mjs`,
  needs `dsymutil target/release/otter` first) — but this session is
  conformance, not perf.
- Breaking changes are OK (single binary).

## Key files
- `test262_config.toml` — skip_features (proposals) / ignored_tests (path) /
  known_panics. The ONLY place to add skips.
- `crates/otter-test262/` — runner, `config.rs`, `feature_map.rs`.
- `scripts/test262-full-run.sh`, `scripts/test262-safe.sh` (Array heap guard).
- `vendor/test262/test/` — the tests.
- Engine: `crates/otter-vm/src/` (intl/, binary/ for Atomics/TypedArray,
  temporal/, json/, string/, regexp*).

## Definition of done
- Non-Intl, non-staging-proposal sections at ~100% (every remaining failure
  either fixed or has a one-line `ignored_tests`/`skip_features` entry citing a
  concrete upstream bug or proposal-stage reason).
- Intl: explicit decision recorded (milestone vs scoped-partial).
- `ES_CONFORMANCE.md` regenerated; gates green.
