# `otter-test262`

Test262 conformance runner for the new-engine
[`crates/*`](../) Otter stack.

This crate speaks the active `otter-runtime` / `otter-vm` ABI and is the
project's Test262 runner.

## Reports and publication

The one current JSON report owns `tests: Vec<TestResult>`, with every selected
path, esid, features, complete outcome diagnostics and elapsed milliseconds.
`totals` and `by_section` derive from those rows and are checked on loading.
Skips contain a typed `reason`: feature, flag, ignored pattern, known-panic
pattern, source-size bound, missing frontmatter or no strictness variant.
There is no aggregate-only reader or inferred pass for an absent path.

`runner` identifies the actual worker executable by SHA-256, Cargo target and
Rust debug assertions. `semantic_config` records effective timeout, heap cap,
tier, snapshot mode and ordered skip lists; whitelisted inherited GC controls
are represented by value digests. No arbitrary environment values are saved.
The parent freezes config before worker launch and rejects malformed JSONL,
wrong indices or paths, duplicate rows and unexpected holes in a worker prefix.

`merge`, `site` and `conformance` require the exact full pinned corpus, matching
runner provenance and unique paths across every outcome. `just test262-site`
and `just test262-conformance` both use the same release runner as full-run.
For a checked capture, invoke `site` or `conformance` with the retained checked
runner directly; rebuilding or selecting another profile requires a new capture. A missing/crashed batch
prevents publication. The full-run script uses `--jobs 1` and a finite outer
batch watchdog (`BATCH_TIMEOUT`, default 3600 seconds). `diff` compares exact identical selections and semantic
conditions, retains both executable identities, and rejects new/changed skips
as regressions. Intentional engine binary changes remain comparable.

The old public aggregate report is unpublished until a genuine complete report
is generated. Historical numbers in `ES_CONFORMANCE.md` remain historical
measurements; omitted rows are not fabricated.

## Quick start

```sh
# One-time: vendor the test262 corpus (a git submodule).
git submodule update --init --recursive vendor/test262

# Walk the corpus without executing anything .
cargo run -p otter-test262 -- run --dry-run

# `just` shortcut.
just test262-dry
```

The runner refuses to launch when `vendor/test262/test/` is missing
or empty — initialise the submodule first.

## Configuration

`test262_config.toml` (the file the project has used since the legacy
runner) is the single source of truth for skip-lists, skipped
frontmatter flags, ignored test patterns, known-panic patterns, and
the per-test heap cap. The
runner reads it from the repository root by default; pass
`--config <path>` to override.

The shape is:

```toml
timeout_secs = 10
max_heap_bytes_per_test = 536870912
skip_features = ["Atomics", "SharedArrayBuffer", ...]
skip_flags = []
ignored_tests = ["staging/sm/Math", ...]
known_panics = ["S15.10.2.8_A3_T15", ...]
```

CLI flags (`--timeout`, `--max-heap-bytes`, `--filter`) always win
over the config defaults.

## Safety controls

The runner runs under three layers of protection:

1. **In-engine cooperative cancellation.** Per-test wall-clock
   budget; a watchdog thread trips
   `Interpreter::interrupt_handle().interrupt()` on fire. Defaults:
   5 s by default, up to 30 s. Override via `--timeout` /
   `OTTER_TEST262_TIMEOUT_MS`.
2. **Per-test heap cap.** Default 512 MiB. Surfaces as a catchable
   `RangeError("out of memory: heap limit exceeded")` per the
   `MemoryManager` plumbing on `Runtime`. Override via
   `--max-heap-bytes` / `OTTER_TEST262_HEAP_BYTES`.
3. **Process-level wall + memory backstop.** Workers are forked
   processes; `bash scripts/test262-safe.sh` applies `ulimit -v`
   on Linux. The supervisor bounds both startup and periods without a completed test.

Operator rules (ported from `MEMORY.md`):

- **Never** run multiple test262 runners in parallel — they share
  the host memory budget.
- **Never** run with timeouts longer than 30 s per test unless
  explicitly asked (`feedback_no_long_test262.md`).
- Capture production evidence with the intended release or checked binary;
  the report exposes its effective assertion setting.

## Spec links

- ECMA-262: <https://tc39.es/ecma262/>
- Test262 INTERPRETING.md:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
