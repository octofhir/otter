# Task 06 — Spec: Bytecode Dump, Disassembly, and Trace Formats

## Goal

Write `docs/new-engine/specs/bytecode-dump-disasm-trace.md`. This spec freezes
the **output formats** for:

1. Human-readable bytecode disassembly (`otter --dump-bytecode`).
2. Machine-readable bytecode dump (`otter --dump-bytecode=json`).
3. Instruction trace events (`otter --trace [--trace-file ...]`).

Formats must be pinned **before** the first instruction lands, so every
opcode added later inherits a single canonical shape.

## Scope

### 1. Disassembly (text)

Per-function block format:

```
function <name> @ <module>:<startLine>:<startCol>-<endLine>:<endCol>
  registers: <local_count>+<scratch_count>
  upvalues:  <count>
  feedback:  <count>
  source:
    <one-line snippet>
  bytecode:
    <pc>:  <opcode-mnemonic>  <operands>     ; <comment>
    ...
  constants:
    [0] <kind> <value-preview>
    ...
  source_spans:
    pc <pc> -> <line>:<col>-<line>:<col>
```

Rules:

- `pc` is decimal, fixed-width to 6 digits.
- `opcode-mnemonic` is `SnakeUpper` (`LOAD_INT8`, `JUMP_IF_FALSE`, …) and
  matches the canonical name in the bytecode crate.
- Operands use named labels (`r0`, `k[3]`, `+5`, `fb[7]`) — never raw bytes.
- Comments after `;` are optional and may include resolved constant
  previews truncated to 60 chars.
- Source spans are listed as a separate table; lines with no span omit the
  comment.
- Disassembly is deterministic: identical input bytecode produces identical
  text. Used by `otter test` golden files.

### 2. Machine-readable dump (JSON)

`otter --dump-bytecode=json` emits one JSON document per top-level script /
module. Schema (top level):

```json
{
  "otterBytecodeDumpVersion": 1,
  "module": "<specifier>",
  "source_kind": "javascript" | "typescript",
  "functions": [ <Function>, ... ],
  "constants": [ <Constant>, ... ]
}
```

`Function`:

```json
{
  "id": <u32>,
  "name": "<string>",
  "span": [<startOffset>, <endOffset>],
  "loc":  { "start": [<line>, <col>], "end": [<line>, <col>] },
  "registers": { "locals": <u16>, "scratch": <u16> },
  "upvalues":   <u16>,
  "feedback":   <u16>,
  "code": [ <Instruction>, ... ],
  "spans":      [ <SpanEntry>, ... ]
}
```

`Instruction`:

```json
{
  "pc":      <u32>,
  "op":      "<MNEMONIC>",
  "operands":[ { "name": "...", "kind": "...", "value": ... }, ... ]
}
```

`SpanEntry`:

```json
{ "pc": <u32>, "span": [<startOffset>, <endOffset>] }
```

`Constant`:

```json
{ "index": <u32>, "kind": "string"|"number"|"bigint"|"regexp"|"function_id",
  "value": <typed value> }
```

Rules:

- `otterBytecodeDumpVersion` must increment on incompatible changes.
- All offsets are byte offsets into the **original** source (post-erasure
  for TypeScript erasures: see ADR-0002 for what survives).
- All field names are `snake_case`.
- Numbers use IEEE-754 doubles; BigInts are encoded as `{ "bigint": "<dec>" }`
  to avoid JSON precision loss.
- The dump is sorted by function `id`, then `pc`. No iteration-order
  surprises.

### 3. Instruction trace

Trace events follow the **Chrome Trace Event** format
(`{ "traceEvents": [ ... ], "displayTimeUnit": "ns" }`) so they open in
Chrome DevTools Performance and Perfetto unmodified. This matches the
existing project convention (`AGENTS.md` debugging section).

Event kinds emitted by the new engine:

- `vm.instruction` — phase `i` (instant), per executed instruction (only
  when `--trace` is on). Args:
  ```json
  { "pc": <u32>, "op": "<MNEMONIC>", "fn": <function_id>,
    "module": "<specifier>", "span": [s, e] }
  ```
- `vm.call` / `vm.return` — phase `B` / `E` for nested call frames.
- `vm.gc` — phase `B`/`E`, args include `live_bytes`, `total_bytes`.
- `vm.compile` — phase `B`/`E`, args include `function_id`, `bytecode_size`.

Top-level wrapper:

```json
{
  "otterTraceSchemaVersion": 1,
  "displayTimeUnit": "ns",
  "traceEvents": [ ... ]
}
```

Rules:

- Timestamps (`ts`) are monotonic ns since runtime start, not wall clock.
- `--trace-filter <regex>` filters by `args.module` or `args.fn` name; never
  by mnemonic alone.
- Trace files default to `otter-trace.json`. If the path ends in `.txt`, a
  text fallback may be emitted, but the JSON form is canonical.
- The new engine **does not** introduce an Otter-only primary trace format.
  Reuse Chrome Trace Event because that is the project rule (`AGENTS.md`).

### 4. Stability and versioning

- All three formats carry a version field.
- A bump is required when any of these change:
  - field rename / removal
  - operand encoding
  - mnemonic rename
  - new top-level event categories
- Adding a new optional field is **not** a bump.
- Each version bump is recorded in a `CHANGELOG.md` next to the spec
  (created lazily — first bump creates it).

### 5. Snapshot testing rules

- Disassembly text is consumed by `otter test` golden files. Tests must
  pin the version of the formatter, not the version of the engine.
- JSON dumps used in tests are pretty-printed with 2-space indentation, sort
  order as defined above. The runner provides a normalizer.

### 6. Non-goals

- **Not** a stable on-disk bytecode cache format. (That is a separate spec
  later, with a different version number.)
- **Not** a profiler protocol. CPU profiles continue to use the existing
  `*.cpuprofile` schema documented in `AGENTS.md`.
- **Not** a debugger wire protocol (CDP). That is out of foundation scope.

## Out of scope

- Implementing any formatter or trace sink. (Task `07` lands the minimal
  emitters once the harness exists.)
- Designing opcodes. The slice tasks add opcodes one family at a time.

## Files / directories you may touch

- Create: `docs/new-engine/specs/bytecode-dump-disasm-trace.md`
- Read-only: everything else

## Acceptance criteria

- Spec file exists with sections 1–6 fully populated.
- Both human and machine-readable disassembly formats are illustrated with
  at least one fully worked example each.
- Trace event categories are enumerated with their `args` shapes.
- Version fields are named: `otterBytecodeDumpVersion`,
  `otterTraceSchemaVersion`.
- Non-goals section explicitly rejects on-disk bytecode caching and CDP.

## Verification commands

```bash
test -f docs/new-engine/specs/bytecode-dump-disasm-trace.md
rg -n "otterBytecodeDumpVersion|otterTraceSchemaVersion" \
    docs/new-engine/specs/bytecode-dump-disasm-trace.md
rg -n "vm\.instruction|vm\.call|vm\.return|vm\.gc|vm\.compile" \
    docs/new-engine/specs/bytecode-dump-disasm-trace.md
```

## Risks

- **Format thrash.** Adding fields is fine; renaming them after fixtures
  exist is expensive. Be conservative on names.
- **Performance.** Per-instruction trace events are huge. Document that
  `--trace` is off by default and that filters are applied before encoding.
- **Format split.** Resist adding "lightweight" trace formats. One JSON, one
  text, both versioned.

## Next task

Proceed to [`07-vm-harness-minimal-interpreter.md`](./07-vm-harness-minimal-interpreter.md).

## Status

- **done**
- last update: 2026-04-26
- artifacts: [`docs/new-engine/specs/bytecode-dump-disasm-trace.md`](../specs/bytecode-dump-disasm-trace.md)
- verification:
  - spec exists.
  - `otterBytecodeDumpVersion` / `otterTraceSchemaVersion` mentioned
    (6 hits combined).
  - `vm.instruction|vm.call|vm.return|vm.gc|vm.compile` events all
    enumerated (9 hits, plus `vm.diagnostic` as a sixth event kind).
- decisions locked:
  - text disassembly format: deterministic, banner with `v1`,
    fixed-width pc, named operands, separated source-span table.
  - JSON dump: top-level `otterBytecodeDumpVersion: 1`, snake_case
    fields, sorted by id/pc/index, lossless string encoding via
    `{"utf16":[...]}`, `NaN`/`Infinity` as JSON strings.
  - JSON dump includes `ts_erasures` array per ADR-0002 §4.
  - trace: Chrome Trace Event format (`otterTraceSchemaVersion: 1`)
    with 6 event kinds: `vm.instruction`, `vm.call`, `vm.return`,
    `vm.gc`, `vm.compile`, `vm.diagnostic`.
  - versioning: bump rules enumerated; additive changes are not
    bumps.
  - non-goals: no on-disk bytecode cache format, no CDP, no heap
    snapshot schema, no Otter-only primary trace format.
