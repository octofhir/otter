# Task 04 — ADR-0003: Public Rust API and CLI Shape

## Goal

Write `docs/new-engine/adr/0003-public-api-and-cli.md`. This ADR freezes the
**shape** (not the implementation) of:

- the public Rust embedding API for the new engine; and
- the CLI command contract for the new engine binary.

It mirrors the API/CLI requirements in §M0 and §M1 of the foundation plan and
turns them into a reviewable contract before any code is written.

## Scope

### Public Rust API

Document the following types and methods. For each item, state: stability tier
(`stable`, `experimental`, `internal`), a one-line purpose, and the
non-goals. **No** function bodies, **no** code samples beyond signatures.

- `Runtime` (stable):
  - `Runtime::builder() -> RuntimeBuilder`
  - `Runtime::run_script(&mut self, source: SourceInput, specifier: &str) ->
    Result<ExecutionResult, RuntimeError>`
  - `Runtime::run_module(&mut self, specifier: &str) ->
    Result<ExecutionResult, RuntimeError>`
  - `Runtime::eval(&mut self, source: SourceInput) ->
    Result<ExecutionResult, RuntimeError>` (experimental)
  - `Runtime::interrupt_handle(&self) -> InterruptHandle`
- `RuntimeBuilder` (stable):
  - `with_capabilities(CapabilitySet)`
  - `with_max_heap_bytes(u64)`
  - `with_timeout(Duration)`
  - `with_module_loader(Box<dyn ModuleLoader>)`
  - `with_console(Box<dyn Console>)`
  - `with_trace_sink(Box<dyn TraceSink>)` (experimental)
  - `with_profiling(ProfilingConfig)` (experimental)
  - `build() -> Result<Runtime, BuildError>`
- `SourceInput`:
  - constructors `from_javascript(text)`, `from_typescript(text)`,
    `from_path(path)` (auto-detect by extension)
  - the `.ts` / `.mts` / `.cts` extensions are first-class — no plugin
    required.
- `ExecutionResult`:
  - completion value (opaque `Value` handle, internal until promoted)
  - diagnostics
  - stdout/stderr captures (when configured)
  - timing
  - optional profiling outputs handle
- `Diagnostic`:
  - source span
  - code-frame data (text + caret range)
  - cause chain
  - machine-readable `kind` (enum `DiagnosticKind`)
- `CapabilitySet`:
  - deny-by-default
  - `allow_read(paths)`, `allow_write(paths)`, `allow_net(hosts)`,
    `allow_env(vars)`, `allow_subprocess(bool)`, `allow_ffi(bool)`
- `InterruptHandle`:
  - `interrupt()`, `is_interrupted() -> bool`
- `RuntimeError` (top-level error, `#[non_exhaustive]`):
  - `Compile(Diagnostic)`
  - `Runtime(Diagnostic)`
  - `Timeout`
  - `OutOfMemory`
  - `Capability { capability: &'static str }`
  - `Internal(...)` reserved for bugs only
- Internal-only modules (do **not** expose):
  - bytecode encoding
  - heap handles, GC roots
  - object shapes
  - frame internals

### CLI surface

Document `otter` (or the staging binary name from ADR-0001) commands and
flags. Each command spec includes: synopsis, exit codes, JSON-mode schema
pointer (to the spec written in task `05`), and which Rust API method it
calls. **No** implementation, **no** clap derives.

Commands:

- `otter run <file> [args...]`
- `otter <file> [args...]` (shorthand)
- `otter eval '<expr>'`
- `otter -e '<expr>'` (alias of eval)
- `otter -p '<expr>'` (eval + print final value)
- `otter check <file>`
- `otter test [path] [--suite engine|smoke|test262] [--filter <pat>]
  [--json] [--bless]`
- `otter info [--json]`
- `otter --dump-bytecode <file>` and `--dump-bytecode=json` (machine
  readable; format spec in task `06`)
- `otter --trace [<file>] [--trace-file <out>] [--trace-filter <re>]`
  (format spec in task `06`)

Common flags (every command that runs code):

- `--timeout <duration>`
- `--max-heap-bytes <n>` (`0` = unlimited)
- `--allow-read=<paths>`, `--allow-write=<paths>`, `--allow-net=<hosts>`,
  `--allow-env=<vars>`, `--allow-run`, `--allow-all`
- `--cpu-prof [--cpu-prof-dir <dir>]`
- `--json` (where applicable; schema documented in task `05` or `06`)

CLI rules:

- Every CLI command **must** be implemented as a thin wrapper over the public
  Rust API. The CLI may not call private VM entry points.
- User-visible errors are structured `Diagnostic`s rendered via the CLI
  formatter, not `Debug`-printed enum variants.
- Exit codes:
  - `0` — success
  - `1` — JS thrown error / failing test
  - `2` — usage / argument error
  - `3` — capability denied
  - `4` — timeout
  - `5` — out of memory
  - `64`+ — internal error
- The legacy `otter` binary from `crates/otterjs` keeps its current CLI until
  the new binary is promoted. This ADR governs only the new binary.

## Out of scope

- Implementing any of these types or commands. (Task `07` builds the harness
  that hosts the first version.)
- Choosing the staging binary name. (Done in ADR-0001.)
- Defining the JSON schema for `otter test` and `--dump-bytecode`. (Tasks
  `05` and `06`.)

## Files / directories you may touch

- Create: `docs/new-engine/adr/0003-public-api-and-cli.md`
- Read-only: everything else

## Acceptance criteria

- ADR exists with full type/method list above.
- Each method/type has a stability tier and a one-line purpose.
- Every CLI command lists the Rust API method it dispatches to.
- Exit codes are enumerated.
- The "Internal-only modules" section explicitly names which engine guts stay
  hidden behind unstable modules until promoted.
- No `Cargo.toml` is modified.

## Verification commands

```bash
test -f docs/new-engine/adr/0003-public-api-and-cli.md
rg -n "RuntimeBuilder|SourceInput|ExecutionResult|CapabilitySet|InterruptHandle" \
    docs/new-engine/adr/0003-public-api-and-cli.md
rg -n "otter run|otter eval|otter check|otter test|--dump-bytecode|--trace" \
    docs/new-engine/adr/0003-public-api-and-cli.md
```

## Risks

- **Premature stability.** Marking too much as `stable` locks the engine into
  decisions before they are tested. Default to `experimental` and promote.
- **Leaky internals.** Methods like `Runtime::eval` are inherently dangerous
  to stabilize. Keep them experimental and document the non-goals
  prominently.
- **CLI / API drift.** The single rule that prevents this is: the CLI calls
  the public API. Restate it in the ADR's consequences section.

## Next task

Proceed to [`05-spec-otter-test-harness.md`](./05-spec-otter-test-harness.md).

## Status

- **done**
- last update: 2026-04-26
- artifacts: [`docs/new-engine/adr/0003-public-api-and-cli.md`](../adr/0003-public-api-and-cli.md)
- verification:
  - ADR exists.
  - `rg -n "RuntimeBuilder|SourceInput|ExecutionResult|CapabilitySet|InterruptHandle"`
    — 28 mentions; every required public type covered.
  - `rg -n "otter run|otter eval|otter check|otter test|--dump-bytecode|--trace"`
    — 12 mentions; every required CLI command covered.
- decisions locked:
  - Public API crate: `crates-next/otter-runtime`. CLI crate:
    `crates-next/otter-cli`. CLI depends only on `otter-runtime`.
  - Stability tiers: `stable` / `experimental` / `internal`. Stable
    list is intentionally short for foundation; promotions require
    ADR amendments.
  - Two-layer surface: Layer A (`Otter` zero-config wrapper) and
    Layer B (`Runtime` + `RuntimeBuilder`). Layer A is the obvious
    starting point; Layer B is opt-in for advanced configuration.
  - **Single error enum**: `OtterError` (`#[non_exhaustive]`,
    `thiserror::Error`, `serde::Serialize` / `Deserialize`,
    externally-tagged JSON wire format with
    `error_schema_version: 1`). No `Box<dyn Error>` anywhere on the
    public API. No separate `BuildError` / `RuntimeError`.
  - Public types: `Otter`, `Runtime`, `RuntimeBuilder`, `SourceInput`,
    `ExecutionResult`, `Diagnostic`, `DiagnosticKind`,
    `CapabilitySet`, `InterruptHandle`, `ModuleLoader`, `Console`,
    `TraceSink`, `OtterError`, `ConfigError`, `IoErrorKind`,
    `ProfilingConfig`, `ProfileArtifact`, `StackFrame`, `SourceSpan`.
  - Internal modules (not exposed): bytecode encoding, heap handles,
    GC roots, shapes, frame internals, AST traversal helpers,
    TypeScript erasure pass internals.
  - CLI commands: `run`, `<file>` shorthand, `eval`, `-e`, `-p`,
    `check`, `test`, `info`, plus `--dump-bytecode`, `--trace`.
  - CLI flags: `--timeout`, `--max-heap-bytes`, `--allow-*`,
    `--allow-all`, `--cpu-prof`, `--json`. Defaults: timeout 30 s,
    heap cap 256 MiB, deny-all.
  - Exit codes: 0 success, 1 JS/test failure, 2 usage, 3 capability,
    4 timeout, 5 OOM, 64+ internal.
  - Versioning: `0.1.0` start; stable tier rules apply pre-1.0; full
    semver guarantees from `1.0.0` (post-foundation).
  - Documentation: `///` on every public item; `cargo doc` in CI
    once first stable item lands.
