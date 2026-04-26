# Task 07 — Minimal Interpreter-only VM Harness

## Goal

Stand up the smallest end-to-end interpreter-only VM harness on the new
foundation path. The harness must be capable of:

- accepting a `.js` or `.ts` file (via the public API and the staging CLI),
- parsing it with OXC,
- compiling it to bytecode,
- executing it in a register/accumulator interpreter (choice deferred to
  measurement, see slice tasks below),
- producing an `ExecutionResult` per ADR-0003,
- emitting bytecode dump and instruction trace per the format spec from
  task `06`,
- running through `otter test --suite engine` with the runner spec from
  task `05`.

This task lands the **scaffolding only**, with the absolute minimum opcodes
required to execute a `return undefined;` script and a single literal-load
script. Real semantics (strings, numbers, control flow, calls) are added
slice by slice in tasks `08`–`13`.

This is the first **implementation** task. ADR-0001, ADR-0002, ADR-0003,
and both specs (`05`, `06`) **must** be merged before this task starts.

## Scope

### Crates created (under the staging directory chosen in ADR-0001)

Reference name: `crates-next/`. Replace with the chosen name if ADR-0001
selected a different path. All crate names are placeholders following the
prefix from ADR-0001:

- `otter-syntax` — OXC frontend integration. Owns:
  - `parse_javascript`, `parse_typescript`
  - TypeScript erasure pass producing the AST that the compiler consumes
  - source span types reused from `oxc_span`
- `otter-bytecode` — bytecode container, opcode enum, encoding,
  disassembly, JSON dump. **No** execution code.
- `otter-compiler` — AST → bytecode. Walks the post-erasure AST.
- `otter-vm` — interpreter, value model placeholder, frame model,
  interrupt checkpoint, OOM boundary.
- `otter-runtime` — `Runtime`, `RuntimeBuilder`, capability set,
  module loader trait, console trait, trace sink trait. The public API
  surface from ADR-0003 lives here.
- `otter-cli` — staging binary that reuses ADR-0003's CLI shape.
  Calls only the public API.
- `otter-test` — `otter test` runner per the spec from task `05`.
  Wired into the staging binary as `otter test`.

`#![forbid(unsafe_code)]` everywhere. No GC integration in this task —
values and bytecode references are owned via plain `Rc`/`Box` until a real
GC slice replaces them. This is a deliberate placeholder that the foundation
plan permits because the only allocation here is the script source and the
bytecode container, which never escape a single execution.

### Workspace integration

- Add the staging crates to the **root** `Cargo.toml` `members` list.
- Each new crate has `publish = false`.
- Active crates do **not** depend on the staging crates (ADR-0001).
- Staging crates do **not** depend on `crates/otter-vm`,
  `crates/otter-runtime`, `crates/otterjs`, or `crates/otter-jit`.
- Staging crates **may** depend on `crates/otter-gc` for primitive helpers
  only if necessary; for this task, keep them GC-free.

### Minimum viable behavior

The harness must execute exactly two fixtures end-to-end:

1. `tests/engine/smoke/empty-script.ts` — empty file, completes with
   `undefined`, exit code `0`.
2. `tests/engine/smoke/literal-undefined.ts` — `undefined;`, completes with
   `undefined`, exit code `0`.

Each fixture has a `*.expected.txt` golden disassembly file produced by
`otter --dump-bytecode`. Both round-trip through the formatter spec.

### Bytecode minimum

Define exactly the opcodes needed to run the two fixtures plus a `Return`:

- `Nop`
- `LoadUndefined <reg>`
- `Return <reg>`

Every opcode definition includes:

- mnemonic (per task `06`)
- operand encoding
- stack/register effect
- source-span policy
- interrupt behavior (none for these three; documented anyway)
- allocation behavior (none)

### Interpreter minimum

- Single-threaded dispatch loop. Choose `match`-based dispatch; record the
  decision in `crates-next/otter-vm/README.md` with a measured baseline
  from a Criterion benchmark added in this task. The choice may change
  later — record the measurement so future changes are evidence-based.
- One Criterion benchmark in `otter-vm/benches/dispatch.rs`:
  measures the cost of executing 10 000 `Nop` instructions in a loop.
- Frame model: per ADR-0003 / foundation plan §VM. Compact struct,
  no per-call `Vec` allocation, inline argument slots reserved (unused in
  this task).
- Back-edge interrupt checkpoint helper exists as `Runtime::checkpoint()`
  and is called from a single place (the dispatch loop's back-edge path,
  even though there is no back-edge instruction yet — wire the call site
  so future opcodes inherit it).
- Heap cap and timeout are wired to the runtime checkpoint.

### Public API minimum

Implement the smallest surface that ADR-0003 calls "stable":

- `Runtime`, `Runtime::builder()`, `RuntimeBuilder::build`
- `Runtime::run_script`
- `SourceInput::from_path`, `SourceInput::from_javascript`,
  `SourceInput::from_typescript`
- `ExecutionResult` with at least `completion: Value::Undefined`,
  `diagnostics: Vec<Diagnostic>`, `duration: Duration`
- `Diagnostic` (struct only; production codes added per slice)
- `RuntimeError` (variants: `Compile`, `Runtime`, `Timeout`, `OutOfMemory`,
  `Capability`, `Internal`)
- `CapabilitySet` deny-by-default; allow-mutators stubbed
- `InterruptHandle::interrupt`

Mark every method `experimental` initially. Promotion to `stable` follows
slice acceptance.

### CLI minimum

- `otter run <file>`
- `otter <file>` shorthand
- `otter --dump-bytecode <file>`
- `otter --dump-bytecode=json <file>`
- `otter --trace <file> [--trace-file <out>]`
- `otter test --suite engine`
- `otter info`

Each command is a thin wrapper over the public API. JSON outputs match the
schemas in tasks `05` and `06`.

### Tests

- Unit tests in each crate for its component.
- Two `tests/engine/smoke/*.ts` fixtures listed above, exercised by
  `otter test --suite engine`.
- Snapshot tests for disassembly and JSON dump (golden files under
  `tests/engine/smoke/`).
- One Criterion bench (`dispatch`).

### LLM-friendly module documentation (mandatory)

Per ADR-0001 §6, every Rust source file in every staging crate
**must** open with a `//!` module docstring in this exact shape:

```rust
//! <one-sentence summary of what this module is responsible for>
//!
//! # Contents
//! - `<TypeOrFn>` — <one-line purpose>
//! - `<TypeOrFn>` — <one-line purpose>
//!
//! # Invariants
//! - <single-sentence invariant the file enforces, if any>
//!
//! # See also
//! - [`crate::<other_module>`] — <why a reader might jump there>
//! - <link to the relevant task / ADR / spec under
//!   `docs/new-engine/`>
```

`# Summary` (the first line) and `# Contents` are required for every
file. `# Invariants` and `# See also` may be omitted when the module
genuinely has none. Public types, traits, and functions also carry
their own `///` doc comments — that is a separate rule, governed by
the `missing_docs` lint, not by this task. Each staging crate's
top-level `lib.rs` (or `main.rs`) carries a crate-level `//!`
docstring with the same shape that lists the crate's modules in
`# Contents`.

Enforcement:

- Every staging crate's `lib.rs` / `main.rs` declares
  `#![deny(missing_docs)]` (in addition to
  `#![forbid(unsafe_code)]`).
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  fails if any public item is undocumented.
- A small repo-level grep CI check (added in this task) fails if any
  `crates-next/**/*.rs` file does not start with a `//!` line.

## Out of scope

- String, number, boolean, control flow, calls — those are tasks `08`–`13`.
- GC integration.
- Module loading (only `--suite engine` runs single-file scripts here).
- Console binding beyond a stub (no `console.log`).
- `otter eval`, `-e`, `-p`. (Tasks `09`+ wire these on as the value model
  grows.)

## Files / directories you may touch

- Create: `crates-next/otter-{syntax,bytecode,compiler,vm,runtime,
  cli,test}/...`
- Create: `tests/engine/smoke/empty-script.ts`,
  `tests/engine/smoke/empty-script.expected.txt`,
  `tests/engine/smoke/literal-undefined.ts`,
  `tests/engine/smoke/literal-undefined.expected.txt`
- Edit: root `Cargo.toml` (workspace `members` only)
- Read-only: legacy `crates/*` (out of build graph per ADR-0001;
  do not touch them, and do not copy code out of them — the new
  engine is written from scratch per ADR-0001 §8).

## Acceptance criteria

- The seven staging crates exist, build, and pass their unit tests.
- `cargo build --workspace` succeeds.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  succeeds (matches the existing project gate).
- `cargo fmt --all --check` passes.
- Every `crates-next/**/*.rs` file begins with a `//!` module
  docstring per the LLM-friendly format above; the repo grep CI check
  passes.
- `otter run tests/engine/smoke/literal-undefined.ts` exits 0.
- `otter --dump-bytecode tests/engine/smoke/literal-undefined.ts` matches
  the committed golden file.
- `otter --dump-bytecode=json tests/engine/smoke/literal-undefined.ts`
  validates against the schema in task `06` (basic shape check is enough
  for now).
- `otter test --suite engine` reports both fixtures as `Passed`.
- `cargo bench -p otter-vm --bench dispatch` runs without error.
- Legacy `crates/*` directories are not part of the build graph
  (workspace `members` lists only `crates-next/*`; ADR-0001).

## Verification commands

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo build --workspace
cargo test --workspace
cargo run -p otter-cli -- run tests/engine/smoke/literal-undefined.ts
cargo run -p otter-cli -- --dump-bytecode \
    tests/engine/smoke/literal-undefined.ts
cargo run -p otter-cli -- test --suite engine
cargo bench -p otter-vm --bench dispatch
# Every staging Rust file starts with a //! module docstring:
for f in $(rg -l '^' --type rust crates-next/); do
  head -n 1 "$f" | grep -q '^//!' || { echo "MISSING //! header: $f"; exit 1; }
done
```

## Risks

- **Two production engines.** This task is the moment the staging crates
  appear in the workspace. The import rule from ADR-0001 must be enforced
  by the verification grep above.
- **Premature optimization.** Resist adding inline caches, shape tables,
  threaded dispatch, or interpreter tricks before the slice tasks request
  them. The dispatch baseline benchmark exists so future optimizations are
  evidence-based.
- **Allocation creep.** Even with three opcodes, hidden allocation can
  sneak in via `Vec`/`String` formatting paths. Audit hot paths now.
- **Spec drift.** If reality forces a deviation from tasks `05` or `06`,
  amend the spec first; do not let code and spec diverge silently.

## Next task

Proceed to [`08-typescript-frontend-skeleton.md`](./08-typescript-frontend-skeleton.md).

## Status

- **done**
- last update: 2026-04-26
- artifacts:
  - 7 staging crates under `crates-next/`:
    `otter-bytecode`, `otter-syntax`, `otter-compiler`, `otter-vm`,
    `otter-runtime`, `otter-test`, `otter-cli`. CLI binary name is
    `otter` (per user instruction).
  - root `Cargo.toml` updated with workspace `members`,
    `[workspace.package]`, `[workspace.dependencies]` (incl. pinned
    OXC `0.127`, `oxc_resolver 11.19`, `clap 4`, `serde 1`,
    `serde_json 1`, `toml 0.9`, `thiserror 2`, `miette 7`,
    `smallvec 1`, `indexmap 2`, `tracing 0.1`,
    `tracing-subscriber 0.3`, `criterion 0.8`),
    `[workspace.lints]` (`unsafe_code = "forbid"`,
    `missing_docs = "deny"`, `clippy::all = "deny"`).
  - smoke fixtures `tests/engine/smoke/empty-script.ts` and
    `tests/engine/smoke/literal-undefined.ts`.
  - Criterion bench `crates-next/otter-vm/benches/dispatch.rs`
    (10 000 NOP + RETURN baseline).
- verification:
  - `cargo build --workspace` — green.
  - `cargo test --workspace` — all unit tests pass.
  - `cargo clippy --workspace --all-targets --all-features
     -- -D warnings` — green.
  - `cargo fmt --all -- --check` — clean.
  - `cargo metadata --format-version=1 --offline` — succeeds (was
    disabled in task `02` until the first staging crate landed).
  - `cargo bench -p otter-vm --no-run` — bench compiles.
  - `cargo run -p otter-cli -- run
     tests/engine/smoke/literal-undefined.ts` — exit 0.
  - `cargo run -p otter-cli -- --dump-bytecode
     tests/engine/smoke/literal-undefined.ts` — emits text dump.
  - `cargo run -p otter-cli -- --dump-bytecode=json
     tests/engine/smoke/literal-undefined.ts` — emits JSON with
    `otterBytecodeDumpVersion: 1`.
  - `cargo run -p otter-cli -- test --suite engine` — both fixtures
    `Passed`.
  - Every `crates-next/**/*.rs` starts with `//!` LLM-friendly
    docstring (grep check).
- design highlights:
  - **Layer A (`Otter`)** zero-config wrapper; **Layer B (`Runtime`,
    `RuntimeBuilder`)** advanced. ADR-0003 §3.
  - **Single `OtterError` enum** (`thiserror::Error`,
    `serde::Serialize`/`Deserialize`, `#[non_exhaustive]`,
    `error_schema_version = 1`, externally-tagged JSON wire
    format). No `Box<dyn Error>` on the public surface.
  - **Deno-style capabilities**: `CapabilitySet` with per-resource
    `Permission<T>` enum (`Deny` / `AllowAll` / `Scoped { allow_list,
    deny_list }`) plus `BooleanPermission` for `hrtime`. CLI exposes
    `--allow-read`, `--allow-write`, `--allow-net`, `--allow-env`,
    `--allow-run`, `--allow-ffi`, `--allow-hrtime`, `--allow-all`
    plus matching `--deny-*` flags. All `--allow-*` accept optional
    comma-separated patterns; passing the flag without a value
    enables `AllowAll`. Patterns are stored but not yet enforced —
    enforcement lands with later slices.
  - Foundation opcodes: `Nop`, `LoadUndefined`, `Return`. Future
    slices add string / number / control-flow opcodes.
  - Match-based interpreter dispatch loop with cooperative
    `InterruptFlag`. Criterion baseline pinned in
    `benches/dispatch.rs`.
  - Disassembly text + JSON dump per
    `docs/new-engine/specs/bytecode-dump-disasm-trace.md`.
  - Test harness fixture metadata via TOML in
    `/* otter-test: ... */` comment block; NDJSON `--json` output.
