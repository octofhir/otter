# Task 52 — Trace event emission

## Goal

Wire the trace events documented in
`docs/new-engine/specs/bytecode-dump-disasm-trace.md` through
the runtime so `otter --trace <file> --trace-file out.json`
produces a real Chrome Trace Event JSON stream.

## Scope

- A `TraceSink` trait already exists on `RuntimeBuilder` — flesh
  out a default file-backed implementation.
- Emit `vm.instruction` (instant, gated behind `--trace`),
  `vm.call` / `vm.return` (bracket events on every push / pop),
  `vm.compile` (bracket per compiled function).
- Filtering: `--trace-filter <regex>` matches against
  `args.module` and `args.fn` before encoding.
- Output writes the wrapper:
  `{ "otterTraceSchemaVersion": 1, "displayTimeUnit": "ns",
  "traceEvents": [...] }`.

## Out of scope

- `vm.gc` events (no GC integration yet).
- `vm.diagnostic` events — wire alongside the exceptions task once
  diagnostics carry frames.

## Files / directories you may touch

- `crates-next/otter-vm/`
- `crates-next/otter-runtime/`
- `crates-next/otter-cli/`
- `tests/engine/trace/`

## Acceptance criteria

- `otter --trace tests/engine/smoke/literal-undefined.ts
  --trace-file /tmp/t.json` writes a valid Chrome Trace Event
  JSON file.
- `--trace-filter` removes events whose `args.module` does not
  match.
- A fixture under `tests/engine/trace/` asserts the schema-version
  and at least one `vm.call` event after running a known script.

## Verification commands

```bash
cargo run -p otter-cli -- --trace --trace-file /tmp/t.json \
    tests/engine/smoke/literal-undefined.ts
```

## Risks

- Per-instruction events are huge; gate emission tightly behind
  `--trace`. Filters apply pre-encode.

## Status

- not started
