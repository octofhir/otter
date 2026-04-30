# Task 102 — Test262 harness + metadata

**Parent:** [100 — Test262 conformance](./100-test262-conformance.md).
**Predecessor:** [101 — Runner skeleton](./101-test262-runner-skeleton.md).
**Successor:** [103 — Outcomes + negative](./103-test262-outcomes-and-negative.md).

## Scope

Parse the `/*--- ... ---*/` YAML frontmatter on every test, build
the harness preamble (`assert.js` / `sta.js` / per-test
`includes`), and compile + run the harness through the existing
[`OtterRuntime` eval hook](../../../crates-next/otter-runtime/src/lib.rs).
No outcome mapping, no execution of the test body — that lands
in slice 103.

## Why

Without metadata the runner cannot filter on `features` / `flags`,
respect `negative`, or know whether to run as `script` vs
`module`. Without the harness preamble, every test's
`assert.sameValue(...)` call would throw `ReferenceError`.

## Deliverables

1. **`metadata.rs`** — `Frontmatter` struct deserialised from the
   `/*--- ... ---*/` block via `serde_yaml`. Fields:
   - `description: Option<String>`
   - `esid: Option<String>`
   - `info: Option<String>` (ignored at runtime, kept for reports)
   - `features: Vec<String>`
   - `flags: Vec<TestFlag>` — enum over `OnlyStrict` /
     `NoStrict` / `Module` / `Raw` / `Async` / `Generated` /
     `CanBlockIsFalse` / `CanBlockIsTrue` / `NonDeterministic`
   - `includes: Vec<String>`
   - `negative: Option<Negative { phase: NegativePhase, type_: String }>`
     where `NegativePhase` is `Parse` / `Resolution` / `Runtime`.
   The parser must accept Test262's loose YAML (multiline strings
   beginning with `|`, top-level keys without quotes, comments).
   Spec link:
   <https://github.com/tc39/test262/blob/main/INTERPRETING.md#metadata>.
2. **`harness.rs`** — concatenates the harness preamble:
   - Always: `vendor/test262/harness/assert.js` then `sta.js`
     (skip when `flags: [raw]`).
   - Then each `includes:` entry resolved against
     `vendor/test262/harness/`.
   - Caches the compiled harness bytecode per worker so the
     work is paid once per (`raw`, includes-set) tuple, not per
     test.
3. **Module / script routing**: `flags: [module]` routes through
   `OtterRuntime::run_module_async`; otherwise `run_script`.
   Wire the eval hook so the harness preamble runs in the same
   realm as the test body — fresh `OtterRuntime` per test.
4. **`feature_map.rs`** — single source of truth for
   `Test262 feature → engine readiness`. Foundation buckets:
   - `Ready`: `BigInt`, `class`, `default-parameters`,
     `destructuring-binding-patterns`, `for-of`, `generators`,
     `async-functions`, `async-iteration`, `Proxy`, `Reflect`,
     `Symbol`, `Symbol.iterator`, `TypedArray`, `ArrayBuffer`,
     `DataView`, `Atomics`, `SharedArrayBuffer`,
     `Intl-enumeration`, `Iterator`, `iterator-helpers`,
     `Promise.allSettled`, `Promise.any`, `Promise.withResolvers`,
     `regexp-lookbehind`, `regexp-named-groups`,
     `regexp-unicode-property-escapes`, `template`, …
   - `NotReady`: `decorators`, `Temporal`, `WeakRef`,
     `FinalizationRegistry`, `import-assertions`,
     `regexp-modifiers`, `Atomics.waitAsync` (if the test asserts
     genuine async behaviour beyond the foundation surface), …
   The runner records `Skipped(feature)` for any test whose
   `features:` intersects `NotReady`.
5. **Strict-mode handling**: foundation is always strict. Tests
   with `flags: [noStrict]` (only) skip with reason
   `"foundation is always strict"`. Tests with `[onlyStrict]`
   run as-is. Tests without flags run once (strict).
6. **Async-test polyfill**: `flags: [async]` appends
   `$DONE` polyfill before the test body — a JS function that
   resolves a runner-side `Rc<RefCell<AsyncOutcome>>`. The
   runner waits up to the per-test timeout for `$DONE()` /
   `$DONE(reason)` to fire. (Outcome mapping lives in slice 103;
   this slice only wires the polyfill.)
7. **Edge-case parsing tests**: a small `tests/` module in the
   crate with sample frontmatter blocks (one per shape — every
   `flags` combination, every `phase` value, multiline `info`,
   missing trailing newline). The parser must round-trip without
   panic.

## Files to touch

- `crates-next/otter-test262/src/metadata.rs`
- `crates-next/otter-test262/src/harness.rs`
- `crates-next/otter-test262/src/feature_map.rs`
- `crates-next/otter-test262/Cargo.toml` (add `serde_yaml`,
  `regex` if not present)
- `crates-next/otter-test262/tests/metadata_parse.rs`

## Sequencing notes

- 101 must be merged. The CLI gains a `parse` subcommand here so
  the parser can be exercised without running anything.
- 103 imports `metadata::Frontmatter` and the harness compiler.

## Gates

- `cargo test -p otter-test262 metadata_parse` covers every
  `flags` combination plus negative shapes.
- `cargo run -p otter-test262 -- parse vendor/test262/test/...`
  pretty-prints the frontmatter.
- `cargo run -p otter-test262 -- run --dry-run --collect-features`
  emits a histogram of every `features:` token in the corpus —
  the runner uses this to spot newly-introduced tokens that need
  a `feature_map.rs` row.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.

## Spec links

- Test262 INTERPRETING.md:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
- Frontmatter spec:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md#metadata>
- ECMA-262 §11.10.x ScriptBody / ModuleBody parse-vs-resolution
  distinction (informs the `phase` semantics):
  <https://tc39.es/ecma262/#sec-scripts>
- ADR-0001:
  [`docs/new-engine/adr/0001-design-discipline.md`](../adr/0001-design-discipline.md)
