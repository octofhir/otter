# `otter-test262`

Test262 conformance runner for the new-engine
[`crates/*`](../) Otter stack.

This crate speaks the active `otter-runtime` / `otter-vm` ABI and is the
project's Test262 runner.

## Status

- **Slice 101 (this slice)** — corpus traversal, `--dry-run`,
  refusal-to-launch when `vendor/test262/` is missing.
- **Slice 102** — YAML frontmatter parser, harness loader,
  `feature_map.rs`, `parse` subcommand.
- **Slice 103** — per-test driver, `Outcome` enum, watchdog +
  heap-cap + `catch_unwind` hardening, worker process isolation,
  curated bring-up subset.
- **Slice 104** — JSON + Markdown writers, `diff <previous>`,
  sharding (`--shard N/M`), supervisor + cursor persistence.
- **Slice 105** — GitHub Actions integration, PR-comment template,
  baseline-bump workflow.

## Quick start

```sh
# One-time: vendor the test262 corpus (a git submodule).
git submodule update --init --recursive vendor/test262

# Walk the corpus without executing anything (slice 101).
cargo run -p otter-test262 -- run --dry-run
# total: 51234

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
   5 s for development, 30 s in CI. Override via `--timeout` /
   `OTTER_TEST262_TIMEOUT_MS`.
2. **Per-test heap cap.** Default 512 MiB. Surfaces as a catchable
   `RangeError("out of memory: heap limit exceeded")` per the
   `MemoryManager` plumbing on `Runtime`. Override via
   `--max-heap-bytes` / `OTTER_TEST262_HEAP_BYTES`.
3. **Process-level wall + memory backstop.** Workers are forked
   processes; `bash scripts/test262-safe.sh` applies `ulimit -v`
   on Linux. A hard-kill backstop fires at `2 × timeout`.

Operator rules (ported from `MEMORY.md`):

- **Never** run multiple test262 runners in parallel — they share
  the host memory budget.
- **Never** run with timeouts longer than 30 s per test unless
  explicitly asked (`feedback_no_long_test262.md`).
- The runner refuses to launch on debug builds without
  `--allow-debug` (slice 105).

## Spec links

- ECMA-262: <https://tc39.es/ecma262/>
- Test262 INTERPRETING.md:
  <https://github.com/tc39/test262/blob/main/INTERPRETING.md>
