# ADR-0003 — Public Rust API and CLI Shape

- **Status:** accepted
- **Date:** 2026-04-26
- **Deciders:** project lead
- **Related:**
  - [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
    §"Architecture Rules" #13–#14, §M1
  - [`docs/new-engine/adr/0001-staging-directory.md`](./0001-staging-directory.md)
  - [`docs/new-engine/adr/0002-oxc-frontend.md`](./0002-oxc-frontend.md)
  - [task 04](../tasks/04-adr-public-api-cli-shape.md)

## Context

The new engine in `crates-next/*` exposes two product surfaces:

- a **public Rust API** for embedders (the same API the new CLI uses);
- a **CLI** binary that delivers `otter`-style developer commands.

The foundation plan requires both to be first-class from the start
(rules #13–#14, §M1). Embedders should see product concepts
(`Runtime`, `Script`, `Module`, `Diagnostic`, `CapabilitySet`,
resource limits) and not VM internals (bytecode encoding, GC handles,
shapes, frame internals). The CLI binary calls only the public API —
no private VM entry points.

This ADR freezes the **shape** (types, methods, command names, flags,
exit codes, stability tiers) before any of it is implemented. Task
`07` is the first implementation task; it follows this ADR.

### Design principle: simple over complete

**The public API must be as small, predictable, and obvious as
possible.** An embedder who has never seen Otter before should be
able to run their first script in five lines:

```rust
use otter_runtime::{Otter, OtterError};

fn main() -> Result<(), OtterError> {
    let mut otter = Otter::new();
    let result = otter.run_file("hello.ts")?;
    println!("{}", result.completion_string());
    Ok(())
}
```

Note: the `?` returns the **concrete** `OtterError` enum from §3.6 —
not `Box<dyn Error>`. Every fallible call in the public API returns
`Result<_, OtterError>` so embedders can `match` on a single, rich,
exhaustive (modulo `#[non_exhaustive]`) error type and the CLI can
serialize it to JSON for `--json` output.

Everything beyond `Otter::new()` — capabilities, custom module loaders,
trace sinks, profiling — is opt-in and lives on a separate builder so
it never gets in the way of the simple case. This principle drives
every design choice below: when in doubt, fewer types, fewer methods,
fewer required arguments, and one well-shaped error enum instead of
trait-object error sprawl.

## Decision

### 1. Stability tiers

Every public type and method declared by this ADR has one of three
tiers. Tiers govern what may be renamed and what may not.

- **stable** — semantic-versioned API surface. Renaming or removing
  any item requires a major-version bump and an ADR amendment.
- **experimental** — `#[deprecated(note = "...")]` not required, but
  documentation explicitly marks the item as experimental. Breaking
  changes are allowed via a single dedicated commit.
- **internal** — visible from the public crate but routed through
  `pub mod __unstable` (or equivalent) so consumers must opt in. May
  be renamed at any time.

Foundation phase defaults to **experimental** for everything except
the items explicitly listed as `stable` below. Promotion to `stable`
happens after a slice ships and the surface has at least one
external consumer.

### 2. Crates that own the public API

- `crates-next/otter-runtime` — owns `Runtime`, `RuntimeBuilder`,
  `SourceInput`, `ExecutionResult`, `Diagnostic`, `CapabilitySet`,
  `OtterError`, `ConfigError`, `IoErrorKind`,
  `InterruptHandle`, `ModuleLoader`, `Console`,
  `TraceSink`, `ProfilingConfig`. This is the crate embedders depend
  on.
- `crates-next/otter-cli` — owns the `otter` binary. Depends only on
  `otter-runtime` (no direct dependency on `otter-vm`,
  `otter-bytecode`, `otter-compiler`, `otter-syntax`).

A future amendment may carve smaller public crates if needed (e.g.,
`otter-runtime-types` for `Diagnostic` and `Span` re-exports). The
foundation phase keeps everything in `otter-runtime`.

### 3. Public Rust API

The API has **two layers**:

- **Layer A — `Otter`**: zero-configuration entry point. Use this
  when you "just want to run a script". One type, one constructor,
  five methods. Sensible defaults. No traits to implement.
- **Layer B — `Runtime` + `RuntimeBuilder`**: opt-in advanced layer.
  Use this when you need custom capabilities, custom module loading,
  trace sinks, profiling, or fine-grained resource limits. Layer A
  is implemented on top of Layer B.

Embedders new to Otter never need to look at Layer B. The CLI uses
Layer B internally; it does so via the same entry points
documented below — no private surface.

#### Layer A: `Otter` (zero-config)

- tier: **stable**
- purpose: the obvious starting point. One type, one constructor,
  one method per common task.
- public surface:

  | Method | Tier | Purpose |
  | --- | --- | --- |
  | `Otter::new() -> Otter` | stable | Construct with safe defaults: deny-all capabilities, 256 MiB heap cap, 30 s timeout, default module loader (`file://` only), stdout/stderr console. |
  | `run_file(&mut self, path: impl AsRef<Path>) -> Result<ExecutionResult, OtterError>` | stable | Read the file, detect kind by extension (`.js`/`.mjs`/`.cjs`/`.ts`/`.mts`/`.cts`), execute it. The vast majority of programs need only this. |
  | `run_script(&mut self, source: &str) -> Result<ExecutionResult, OtterError>` | stable | Run a string of JavaScript. |
  | `run_typescript(&mut self, source: &str) -> Result<ExecutionResult, OtterError>` | stable | Run a string of TypeScript. |
  | `eval(&mut self, source: &str) -> Result<ExecutionResult, OtterError>` | stable | Evaluate a single expression / statement and return its completion value. |
  | `interrupt(&self)` | stable | Cooperative cancellation. Cheap, callable from any thread. |

That is the entire surface for the simple case. No `Box<dyn ...>` to
construct, no builder to chain, no traits to implement.

#### Layer B: `Runtime` + `RuntimeBuilder` (advanced)

When Layer A is not enough, drop down to Layer B. `Otter` is a thin
wrapper around `Runtime` and you can switch at any time without
re-architecting your code: every Layer A method has a Layer B
equivalent that takes more arguments.

##### `Runtime`

- tier: **stable**
- purpose: an isolate. One thread, one runtime. Holds the heap,
  the module registry, the interrupt handle, and the timer queue.
- methods:

  | Method | Tier | Purpose |
  | --- | --- | --- |
  | `Runtime::builder() -> RuntimeBuilder` | stable | Start configuring a new runtime. |
  | `run_script(&mut self, source: SourceInput, specifier: &str) -> Result<ExecutionResult, OtterError>` | stable | Execute a single-file script. |
  | `run_module(&mut self, specifier: &str) -> Result<ExecutionResult, OtterError>` | stable | Execute a module by specifier through the configured `ModuleLoader`. |
  | `eval(&mut self, source: SourceInput) -> Result<ExecutionResult, OtterError>` | stable | Evaluate a script and return its completion. |
  | `interrupt_handle(&self) -> InterruptHandle` | stable | Cheap, cloneable handle for cooperative cancellation. |

Foundation non-goals (kept for clarity):

- `Runtime` is **not** thread-safe. `Send` and `Sync` impls are
  intentionally absent. One runtime per thread.
- `Runtime` is **not** snapshot-restorable. Snapshots are a separate
  later surface.

##### `RuntimeBuilder`

- tier: **stable**
- purpose: a *single* builder that configures everything. Default
  values match `Otter::new()`, so `Runtime::builder().build()` and
  `Otter::new()` produce equivalent runtimes.
- methods (all chainable, all optional):

  | Method | Tier | Purpose |
  | --- | --- | --- |
  | `capabilities(self, caps: CapabilitySet) -> Self` | stable | Replace the capability set. Default: `CapabilitySet::deny_all()`. |
  | `max_heap_bytes(self, bytes: u64) -> Self` | stable | Hard heap cap. `0` disables. Default: 256 MiB. |
  | `timeout(self, timeout: Duration) -> Self` | stable | Per-`run_*` timeout. `Duration::ZERO` disables. Default: 30 s. |
  | `max_stack_depth(self, depth: u32) -> Self` | stable | JS call-stack limit. Default: 1024. |
  | `module_loader(self, loader: Box<dyn ModuleLoader>) -> Self` | stable | Custom loader. Default: `file://` only. |
  | `console(self, console: Box<dyn Console>) -> Self` | stable | Custom console. Default: stdout / stderr. |
  | `trace_sink(self, sink: Box<dyn TraceSink>) -> Self` | experimental | Receive `vm.*` trace events. |
  | `profiling(self, cfg: ProfilingConfig) -> Self` | experimental | Configure CPU sampling. |
  | `build(self) -> Result<Runtime, OtterError>` | stable | Construct the runtime. |

Naming rule: builder methods are **named after what they set**, not
prefixed with `with_`. `caps`, `timeout`, `console` — short and
discoverable in IDE autocomplete.

#### `SourceInput`

- tier: **stable**
- purpose: hand a script / module to the runtime with explicit source
  kind detection so `.ts` is first-class.
- constructors:

  | Constructor | Tier | Purpose |
  | --- | --- | --- |
  | `SourceInput::from_javascript(text: impl Into<Cow<'static, str>>)` | stable | Treat the text as JavaScript. |
  | `SourceInput::from_typescript(text: impl Into<Cow<'static, str>>)` | stable | Treat the text as TypeScript (parsed via OXC TS mode per ADR-0002). |
  | `SourceInput::from_path(path: impl AsRef<Path>)` | stable | Read from disk; source kind is detected from the file extension (`.js`, `.mjs`, `.cjs`, `.ts`, `.mts`, `.cts`). Other extensions return `OtterError::SourceKind { path, extension }`. |

Internal accessors (e.g., `source_kind()`, `text()`) are **internal**.

#### `ExecutionResult`

- tier: **experimental**
- purpose: structured result of one `run_*` / `eval` invocation.
- fields (all read-only via accessors):

  | Field | Tier | Purpose |
  | --- | --- | --- |
  | `completion: Value` | experimental (foundation: opaque) | The completion value of the script / module. The `Value` type itself is internal in the foundation phase — embedders read it via `as_string()`, `as_number()`, `is_undefined()`, etc., promoted per slice. |
  | `diagnostics: Vec<Diagnostic>` | stable | Structured diagnostics emitted during the run. |
  | `stdout: Option<Vec<u8>>` | stable | Captured stdout, when configured. |
  | `stderr: Option<Vec<u8>>` | stable | Captured stderr, when configured. |
  | `duration: Duration` | stable | Wall-clock time. |
  | `profile: Option<ProfileArtifact>` | experimental | Optional CPU profile. |

#### `Diagnostic`

- tier: **stable** (struct shape)
- purpose: structured error report. Same type for compile errors,
  runtime errors, and `otter check` diagnostics.
- fields:

  | Field | Type | Purpose |
  | --- | --- | --- |
  | `kind` | `DiagnosticKind` | Machine-readable category. |
  | `code` | `&'static str` | Stable error code (`TS_UNSUPPORTED`, `OOM_HEAP_LIMIT`, …). |
  | `message` | `String` | Human-readable summary. |
  | `span` | `Option<SourceSpan>` | Original source span (preserved through TypeScript erasure per ADR-0002). |
  | `frames` | `Vec<StackFrame>` | Call stack when relevant. |
  | `cause` | `Option<Box<Diagnostic>>` | Cause chain. |

`DiagnosticKind` is a `#[non_exhaustive]` enum:
`Syntax`, `Type`, `Reference`, `Range`, `OutOfMemory`, `Timeout`,
`Capability`, `Internal`. The list grows; existing variants do not
move tiers.

#### `CapabilitySet`

- tier: **stable**
- purpose: deny-by-default capability bag.
- methods:

  | Method | Tier | Purpose |
  | --- | --- | --- |
  | `CapabilitySet::deny_all()` | stable | Default; no I/O. |
  | `allow_read(self, paths: impl IntoIterator<Item = PathBuf>) -> Self` | stable | Whitelist read paths. |
  | `allow_write(self, paths: impl IntoIterator<Item = PathBuf>) -> Self` | stable | Whitelist write paths. |
  | `allow_net(self, hosts: impl IntoIterator<Item = String>) -> Self` | stable | Whitelist hostnames. |
  | `allow_env(self, names: impl IntoIterator<Item = String>) -> Self` | stable | Whitelist env var names; built-in deny patterns (`AWS_*`, `*_SECRET*`, `*_TOKEN*`, etc.) are always enforced. |
  | `allow_subprocess(self, allow: bool) -> Self` | stable | Toggle subprocess capability. |
  | `allow_ffi(self, allow: bool) -> Self` | stable | Toggle FFI capability. |
  | `allow_all() -> Self` | stable | Convenience for development. The CLI maps `--allow-all` to this. |

#### `InterruptHandle`

- tier: **stable**
- purpose: cooperative cancellation from another thread.
- methods:

  | Method | Tier | Purpose |
  | --- | --- | --- |
  | `interrupt(&self)` | stable | Mark the runtime for interrupt; the next back-edge / native loop checkpoint converts it into a catchable runtime error. |
  | `is_interrupted(&self) -> bool` | stable | Check the flag without resetting it. |
  | `reset(&self)` | experimental | Clear a previous interrupt. Use only if you know no inflight `run_*` saw it. |

#### `ModuleLoader`, `Console`, `TraceSink` traits

- tier: **stable** for `ModuleLoader` and `Console`; **experimental**
  for `TraceSink`.
- purpose: extension points the embedder implements.
- foundation contract is one method each, with full signatures
  documented in the implementation in task `07`. The trait names and
  the responsibility split are locked here.

#### `OtterError` (the only error type embedders see)

- tier: **stable** (the enum carries `#[non_exhaustive]`; new variants
  may be added without a major bump)
- shape: a plain Rust `enum`, derived `Debug`, `Clone`,
  `thiserror::Error`, `serde::Serialize`, `serde::Deserialize`. **No
  `Box<dyn Error>` anywhere on the public API.**
- the **same** `OtterError` is returned by every fallible method in
  Layer A and Layer B (`Otter::*`, `Runtime::*`, `RuntimeBuilder::build`).
  There is no separate `BuildError` and no separate `RuntimeError`.
  One enum, one match site.
- variants:

  | Variant | Tier | Meaning |
  | --- | --- | --- |
  | `Config { reason: ConfigError }` | stable | `RuntimeBuilder::build` failed (invalid limits, unsupported feature combo, conflicting capabilities). |
  | `Io { path: PathBuf, kind: IoErrorKind, message: String }` | stable | File / module loader could not read the input. `IoErrorKind` mirrors a small enum of `NotFound`, `PermissionDenied`, `Other`. |
  | `SourceKind { path: PathBuf, extension: String }` | stable | `Otter::run_file` was given a file with an unsupported extension. |
  | `Compile { diagnostics: Vec<Diagnostic> }` | stable | Parse / TypeScript erasure / lowering produced one or more diagnostics. The vec is non-empty. |
  | `Runtime { diagnostic: Diagnostic }` | stable | A catchable JS error escaped the script. The diagnostic carries `kind`, `code`, `message`, `span`, and `frames`. |
  | `Timeout { elapsed: Duration }` | stable | The configured timeout fired. |
  | `OutOfMemory { requested_bytes: u64, heap_limit_bytes: u64 }` | stable | Heap cap was hit. Allocation was rejected before mutating the heap (foundation rule §9). |
  | `Capability { capability: &'static str, detail: Option<String> }` | stable | A guarded operation (`fs_read`, `fs_write`, `net`, `env`, `subprocess`, `ffi`) was denied. |
  | `Interrupted` | stable | `InterruptHandle::interrupt()` was observed at a checkpoint. |
  | `Internal { message: String, code: &'static str }` | stable | The runtime hit an invariant violation it could not recover from. **Should be rare; CI hard-fail.** Embedders may report this upstream but cannot match on its internal structure. |

  Notes:
  - Each variant uses **named fields**, not tuple variants, so adding
    a field later does not break consumers (combined with
    `#[non_exhaustive]`).
  - `Diagnostic`, `ConfigError`, `IoErrorKind` are concrete types
    with the same derives (`Debug`, `Clone`, `thiserror::Error` for
    `ConfigError`, `serde::Serialize`, `serde::Deserialize`).
  - `OtterError` implements `Display` via `thiserror::Error`. The
    `Display` text is human-friendly (matches the CLI's default
    rendering); for machine output, embedders use `serde_json` or
    the public `OtterError::to_json()` convenience method.

##### Error shape requirements

- **No `Box<dyn Error>` anywhere on the public API.** Internal helpers
  may use `Box<dyn Error>` if needed, but every `pub fn` returns
  `Result<_, OtterError>` directly.
- **No `&dyn Error` on the public API.** Callers traverse the cause
  chain via `OtterError`'s explicit cause field (`cause:
  Option<Box<OtterError>>` is reserved if a future amendment needs
  it; not added in foundation since the existing variants carry
  enough context).
- **Serialization is stable.** `serde::Serialize` and
  `serde::Deserialize` produce the wire shape documented in §3.7.
  Changing the wire shape requires an ADR amendment **and** an
  `error_schema_version` bump.
- **`From` impls.** Limited to internal types. `OtterError` does not
  implement `From<std::io::Error>` (the runtime translates I/O errors
  through the `Io` variant with the source path attached) or
  `From<oxc_diagnostics::Error>` (translated into `Compile { diagnostics }`).
  This keeps the public surface deterministic.

##### `ConfigError`

A small companion enum used by `OtterError::Config { reason }`.

```rust
#[derive(Debug, Clone, thiserror::Error,
         serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[non_exhaustive]
pub enum ConfigError {
    #[error("invalid heap limit: {message}")]
    InvalidHeapLimit { message: String },
    #[error("invalid timeout: {message}")]
    InvalidTimeout { message: String },
    #[error("invalid stack depth limit: {message}")]
    InvalidStackDepth { message: String },
    #[error("conflicting capabilities: {message}")]
    ConflictingCapabilities { message: String },
}
```

##### `IoErrorKind`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq,
         serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
#[non_exhaustive]
pub enum IoErrorKind {
    NotFound,
    PermissionDenied,
    Other,
}
```

The internal mapping from `std::io::ErrorKind` is documented in code
in `otter-runtime`'s `error.rs`.

#### JSON wire format for `OtterError` (§3.7)

`OtterError` derives `serde::Serialize` / `serde::Deserialize` so the
CLI can emit it in `--json` mode and CI / tooling can parse it
without writing any extra translation layer. The wire format is
**externally-tagged**, `snake_case`, and stable under
`error_schema_version`.

Top-level shape:

```json
{
  "error_schema_version": 1,
  "error": <OtterErrorBody>
}
```

`OtterErrorBody` is the serde-tagged enum body. Each variant
becomes a JSON object with a `"kind"` discriminator and the variant's
named fields:

| `kind` | Payload |
| --- | --- |
| `"config"` | `{ "reason": <ConfigError JSON> }` |
| `"io"` | `{ "path": "<string>", "kind": "<IoErrorKind>", "message": "<string>" }` |
| `"source_kind"` | `{ "path": "<string>", "extension": "<string>" }` |
| `"compile"` | `{ "diagnostics": [<Diagnostic JSON>, ...] }` |
| `"runtime"` | `{ "diagnostic": <Diagnostic JSON> }` |
| `"timeout"` | `{ "elapsed_ms": <u64> }` |
| `"out_of_memory"` | `{ "requested_bytes": <u64>, "heap_limit_bytes": <u64> }` |
| `"capability"` | `{ "capability": "<string>", "detail": "<string>\|null" }` |
| `"interrupted"` | `{}` |
| `"internal"` | `{ "message": "<string>", "code": "<string>" }` |

Worked example — running `otter run nope.foo` produces:

```json
{
  "error_schema_version": 1,
  "error": {
    "kind": "source_kind",
    "path": "nope.foo",
    "extension": "foo"
  }
}
```

A compile error from `otter run bad.ts` produces:

```json
{
  "error_schema_version": 1,
  "error": {
    "kind": "compile",
    "diagnostics": [
      {
        "kind": "syntax",
        "code": "TS_UNSUPPORTED",
        "message": "enum is not supported in foundation",
        "span": { "start": 12, "end": 16 },
        "frames": [],
        "cause": null
      }
    ]
  }
}
```

Rules:

- Field order in pretty-printed JSON is the order documented above
  per variant, so golden tests stay stable.
- `Duration` fields serialize as integer milliseconds (`elapsed_ms`,
  `*_ms` suffix) so JSON round-trips through `i64` without precision
  loss.
- `error_schema_version` lives at the top level, not inside the
  variant body, so a tool can `match` on `version` before parsing
  the rest.
- A bump of `error_schema_version` is required when an existing
  variant's `kind`, field name, or field type changes. Adding a new
  variant or a new optional field is **not** a bump (consumers
  handle unknown variants via `#[non_exhaustive]` / `serde(other)`).

#### Internal-only modules (do not expose)

- bytecode encoding and instructions (lives in
  `crates-next/otter-bytecode`)
- heap handles, GC roots
- object shapes / inline cache slots
- frame internals (`CallFrame`, register windows)
- AST traversal helpers used inside `otter-compiler`
- TypeScript erasure pass internals

These crates are part of the workspace (so `cargo test --workspace`
covers them) but their `pub` API is internal: the only consumer
allowed is the `otter-runtime` crate.

### 4. CLI surface

The new `otter` binary lives in `crates-next/otter-cli` and is the
only `otter` binary built by the workspace (legacy `crates/otterjs` is
out of the build graph per ADR-0001).

Every command below is implemented as a thin wrapper over a
`Runtime` method and produces structured output.

#### Commands

| Command | Tier | Calls |
| --- | --- | --- |
| `otter run <file> [args...]` | stable | `Runtime::run_script` / `Runtime::run_module` based on file kind. |
| `otter <file> [args...]` (shorthand) | stable | Same as `otter run`. |
| `otter eval '<expr>'` | experimental | `Runtime::eval`. |
| `otter -e '<expr>'` | experimental | Alias of `otter eval`. |
| `otter -p '<expr>'` | experimental | `eval` + print final value via the formatter. |
| `otter check <file>` | stable | Parse + erase + compile only. No execution. |
| `otter test [path] [--suite engine\|smoke\|test262] [--filter <pat>] [--json] [--bless]` | stable | Engine test harness; spec in [`docs/new-engine/specs/otter-test-harness.md`](../specs/otter-test-harness.md). |
| `otter info [--json]` | stable | Print build/runtime feature flags. |
| `otter --dump-bytecode <file>` | experimental | Disassembly per spec task `06`. |
| `otter --dump-bytecode=json <file>` | experimental | Machine-readable dump per spec task `06`. |
| `otter --trace [<file>] [--trace-file <out>] [--trace-filter <re>]` | experimental | Instruction trace per spec task `06`. |

#### Common flags (every command that runs code)

| Flag | Tier | Default | Purpose |
| --- | --- | --- | --- |
| `--timeout <duration>` | stable | `30s` | Maps to `RuntimeBuilder::with_timeout`. `0` disables the timeout. |
| `--max-heap-bytes <n>` | stable | `268435456` (256 MiB) | Maps to `RuntimeBuilder::with_max_heap_bytes`. `0` disables the cap. |
| `--allow-read=<paths>` | stable | none | Repeatable; merges into `CapabilitySet`. |
| `--allow-write=<paths>` | stable | none | Repeatable. |
| `--allow-net=<hosts>` | stable | none | Repeatable. |
| `--allow-env=<vars>` | stable | none | Repeatable; built-in deny patterns enforced. |
| `--allow-run` | stable | off | Subprocess capability. |
| `--allow-all` | stable | off | Equivalent to `CapabilitySet::allow_all()`. |
| `--cpu-prof [--cpu-prof-dir <dir>] [--cpu-prof-name <name>]` | experimental | off | Maps to `RuntimeBuilder::with_profiling`. |
| `--json` | stable per command | off | Where applicable; output schema documented per command. |

Flag resolution order: built-in defaults < `otter.toml` config <
environment variables < CLI flags. The CLI never consults a global
`/etc/otter.toml` — config search walks up parent directories from
the working directory, identical to the legacy CLI behavior.

#### Exit codes

| Code | Meaning |
| --- | --- |
| `0` | Success. |
| `1` | Catchable JS error / failing test. |
| `2` | Usage / argument error (clap-style). |
| `3` | Capability denied. |
| `4` | Timeout. |
| `5` | Out of memory. |
| `64`+ | Internal error. CI must treat any code in this range as a hard fail. |

#### CLI rules

- Every CLI command **must** be implemented as a thin wrapper over the
  public Rust API in `otter-runtime`. The CLI may not call private
  modules of `otter-vm`, `otter-compiler`, etc.
- User-visible errors are structured `Diagnostic`s rendered through
  the runtime's formatter (which uses `oxc_diagnostics` /
  `miette`-style code frames per ADR-0002), not `Debug`-printed enum
  variants.
- `--json` mode is stable per command. Each `--json`-supporting command
  documents its schema in the relevant spec under
  `docs/new-engine/specs/`. Schema changes require a version field
  bump.
- Every CLI command has at least one integration test under
  `crates-next/otter-cli/tests/`. Slice tasks add command-specific
  fixtures.

### 5. Versioning policy

The new engine's public crate (`otter-runtime`) starts at
`0.1.0`. The CLI binary (`otter-cli`) tracks the same version. While
all `0.x` releases are nominally allowed to break, this ADR's
**stable** tier still applies: a stable item changing requires an ADR
amendment in the same commit.

Once the engine ships its first user-visible release, the major
version bumps to `1.0.0` and the **stable** tier becomes a hard
guarantee under semver rules. That promotion is **not** a foundation-
phase task.

### 6. Documentation contract

- Every public item (per the LLM-friendly rule from ADR-0001 §6) has a
  `///` doc comment with: one-sentence summary, `# Examples` (for
  stable items only — experimental items may skip), `# Errors` for
  fallible methods, `# Panics` if any.
- `otter-runtime`'s `lib.rs` `//!` crate-level docstring lists the
  public types in its `# Contents` block.
- `cargo doc --no-deps -p otter-runtime` is wired into CI and fails on
  broken intra-doc links once the first stable item lands.

## Consequences

For embedders:

- A small, predictable surface: `Runtime`, `RuntimeBuilder`,
  `SourceInput`, `ExecutionResult`, `Diagnostic`, `CapabilitySet`,
  `OtterError`, `InterruptHandle`. Everything else is internal.
- The TypeScript-first promise is encoded in `SourceInput`: there is
  no `with_typescript_loader_plugin` API to forget.

For CLI users:

- Stable command names and exit codes from day one.
- `otter test` is the canonical engine harness, not a Jest clone.
- `otter check`, `otter --dump-bytecode`, and `otter --trace` are
  available from the first VM harness milestone (task `07`).

For contributors:

- Every CLI feature lives in `otter-runtime` first (or in a private
  internal crate that `otter-runtime` re-exports), then in
  `otter-cli`. Reverse direction is forbidden.
- Stability tiers are checked in PR review. Promoting an
  **experimental** item to **stable** requires an ADR amendment.

## Alternatives considered

- **One mega-crate.** Rejected: hides the internal/external boundary.
- **Async public API (`async fn run_script`).** Rejected for the
  foundation phase: the runtime is single-threaded and the timer
  queue is internal. A future amendment may add an async wrapper
  crate (`otter-runtime-async`) without touching this surface.
- **A `Value` type exposed publicly from day one.** Rejected: the
  value representation will evolve heavily through slices `09`–`13`.
  Embedders use accessors (`as_string()`, `as_number()`, …) until the
  representation stabilizes.

## ADR amendments

### 2026-04-26 — Deno-style capability model

- **Change:** the [`CapabilitySet`] type was redesigned around a
  per-resource [`Permission<T>`] enum with three states (`Deny`,
  `AllowAll`, `Scoped { allow_list, deny_list }`) plus a
  [`BooleanPermission`] for `hrtime`. The CLI exposes Deno-style
  paired flags: `--allow-read`/`--deny-read`, `--allow-write`/
  `--deny-write`, `--allow-net`/`--deny-net`,
  `--allow-env`/`--deny-env`, `--allow-run`/`--deny-run`,
  `--allow-ffi`/`--deny-ffi`, `--allow-hrtime`, `--allow-all`. Each
  `--allow-*` accepts an optional comma-separated pattern list;
  passing the flag without a value upgrades to `AllowAll`. `--deny-*`
  always wins on conflict.
- **Reason:** user request — the Deno per-permission model is a
  better fit than a flat capability bag and matches contemporary
  user expectations for runtime permission flags.
- **Linked task:** [task 07](../tasks/07-vm-harness-minimal-interpreter.md).

### 2026-04-26 — UX-first defaults and `--sandbox`

- **Change:** [`CapabilitySet::default`] (used by [`Otter::new`] and
  the runtime builder) now ships **practical defaults** rather than
  deny-all: `read = AllowAll` (so module imports just work),
  `write/net/env/run/ffi = Deny`, `hrtime = Allow`. A new preset
  [`CapabilitySet::sandbox`] denies everything; the CLI maps it to
  `--sandbox`. `--allow-all` and `--sandbox` are mutually exclusive.
- **Reason:** user feedback — forcing every embedder to spell out
  the read paths up front is bad UX for the common case. Power users
  retain scoped pattern lists; everyone else gets a runtime that
  works out of the box.
- **Linked task:** [task 07](../tasks/07-vm-harness-minimal-interpreter.md).

### 2026-04-26 — Glob patterns in string permissions; built-in env secret deny

- **Change:** [`Permission<String>`] now supports glob-style
  patterns (`*`, `PREFIX_*`, `*_SUFFIX`, `PREFIX_*_SUFFIX`) via the
  new `Permission::matches` method. Path patterns
  ([`Permission<PathBuf>::matches_path`]) match by **path prefix**
  (an allow of `/var/data` covers `/var/data/x.json`); wildcards in
  paths are reserved for a future amendment. A new constant
  [`ENV_BUILTIN_DENY_PATTERNS`] lists names that **always** deny,
  even under `--allow-all` — currently `*_SECRET`, `*_TOKEN`,
  `*_PASSWORD`, `*_API_KEY`, `AWS_*`, `GITHUB_TOKEN`,
  `OPENAI_API_KEY`, `ANTHROPIC_API_KEY`. Use
  [`CapabilitySet::env_allows`] to query env permissions; it consults
  the built-in deny list before the user-configured permission.
- **Reason:** user request — VITE-style prefix scoping
  (`VITE_APP_*`, `NEXT_PUBLIC_*`, `OTEL_*`) is a common workflow for
  env vars; built-in secret deny ensures a stray `--allow-all` does
  not exfiltrate credentials.
- **Linked task:** [task 07](../tasks/07-vm-harness-minimal-interpreter.md).

When a slice promotes an experimental item to stable, demotes a
stable item, adds a new public type, or changes an exit code,
append a dated entry of the form:

```markdown
### 20YY-MM-DD — <short title>

- **Change:** <what was added / removed / changed>
- **Reason:** <why>
- **Linked task:** [task XX](../tasks/XX-...)
```

## References

- Foundation plan: [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
- Staging-directory ADR: [`0001-staging-directory.md`](./0001-staging-directory.md)
- OXC frontend ADR: [`0002-oxc-frontend.md`](./0002-oxc-frontend.md)
- Test harness spec: [`docs/new-engine/specs/otter-test-harness.md`](../specs/otter-test-harness.md)
- Bytecode dump / trace spec: [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](../specs/bytecode-dump-disasm-trace.md)
- Task: [`docs/new-engine/tasks/04-adr-public-api-cli-shape.md`](../tasks/04-adr-public-api-cli-shape.md)
