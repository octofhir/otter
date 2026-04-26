# SPEC — Bytecode Dump, Disassembly, and Trace Formats

- **Status:** accepted
- **Date:** 2026-04-26
- **Related:**
  - [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
    §"Foundation Shape", §M2, §"VM and Interpreter Design Bar"
  - [`docs/new-engine/adr/0003-public-api-and-cli.md`](../adr/0003-public-api-and-cli.md)
  - [task 06](../tasks/06-spec-bytecode-dump-disasm-trace.md)

## Purpose

Three observability surfaces on the new engine are pinned **before**
the first opcode lands so every future opcode inherits a single
canonical shape:

1. Human-readable disassembly (`otter --dump-bytecode`).
2. Machine-readable bytecode dump (`otter --dump-bytecode=json`).
3. Instruction / lifecycle trace (`otter --trace`).

These formats are stable enough for snapshot tests and CI parsing.
Adding new fields is fine. Renaming or removing fields requires a
version bump (see §6).

## 1. Disassembly (text)

`otter --dump-bytecode <file>` writes a deterministic text rendering
of the program's bytecode to stdout. Default per-function block:

```
function <name> @ <module>:<startLine>:<startCol>-<endLine>:<endCol>
  registers:  <local_count>+<scratch_count>
  upvalues:   <upvalue_count>
  feedback:   <feedback_slot_count>
  source:
    <one-line snippet, ≤ 120 chars, ellipsis if longer>
  bytecode:
    <pc>:  <MNEMONIC>  <operands>            ; <comment>
    ...
  constants:
    [<index>] <kind> <value-preview, ≤ 60 chars>
    ...
  source_spans:
    pc <pc> -> <line>:<col>-<line>:<col>
```

### 1.1 Rules

- **`pc`** is decimal, fixed-width to 6 digits (`000000`, `000042`,
  `001337`). Width grows in a future bump if 6 digits ever overflow.
- **Mnemonics** are `SCREAMING_SNAKE_CASE` matching the canonical
  name declared by `crates-next/otter-bytecode`. Examples:
  `LOAD_INT8`, `JUMP_IF_FALSE`, `CALL_METHOD`.
- **Operands** use named labels, never raw bytes:
  - `r0`, `r1`, … — registers (locals + scratch)
  - `k[3]` — constant pool index
  - `+5`, `-12` — signed branch offset relative to the **next**
    instruction
  - `fb[7]` — feedback slot index
  - `arg(2)` — argument count
  - `up(1)` — upvalue index
  Multiple operands separated by single spaces.
- **Comments** start with `;` and are optional. They may include:
  - resolved constant previews (`; "abc"` for a string literal load)
  - resolved branch targets (`; -> 000128`)
  - feedback slot purpose (`; ic=named_load`)
  Everything after `;` is informational; tools must not parse it.
- **Source spans** appear as a separate table, not inline. Lines with
  no span are omitted from the table.
- **Determinism.** Identical bytecode produces identical text byte-
  for-byte. The runner pins it via golden tests.

### 1.2 Worked example

For the script `1 + 2`, expected output (illustrative):

```
function <main> @ tests/engine/numbers/add.ts:1:1-1:6
  registers:  0+2
  upvalues:   0
  feedback:   1
  source:
    1 + 2
  bytecode:
    000000:  LOAD_INT8       r0  1                 ; 1
    000003:  LOAD_INT8       r1  2                 ; 2
    000006:  ADD_INT32       r0  r0  r1  fb[0]    ; ic=binop
    000011:  RETURN          r0
  constants:
    (none)
  source_spans:
    pc 000000 -> 1:1-1:2
    pc 000003 -> 1:5-1:6
    pc 000006 -> 1:1-1:6
    pc 000011 -> 1:1-1:6
```

(Mnemonics are illustrative; the actual instruction set is locked
slice-by-slice from task `07` onward.)

### 1.3 Header line

The very first line of the dump is a banner:

```
; otter bytecode dump v1 — module=<specifier> source_kind=<javascript|typescript>
```

This banner is allowed to vary across engine versions in the comment
text after `v1`, but the `v1` token is the dump version and bumping
it follows §6.

## 2. Machine-readable dump (JSON)

`otter --dump-bytecode=json <file>` emits one JSON document per
top-level script / module to stdout (or to a path passed via a future
`--dump-bytecode-file` option).

### 2.1 Top-level shape

```json
{
  "otterBytecodeDumpVersion": 1,
  "module": "<specifier>",
  "source_kind": "javascript",
  "functions": [ <Function>, ... ],
  "constants": [ <Constant>, ... ],
  "ts_erasures": [ <ErasureEntry>, ... ]
}
```

- `otterBytecodeDumpVersion` increments on incompatible changes
  (§6).
- `module` is the script / module specifier.
- `source_kind` is `"javascript"` or `"typescript"`.
- `functions` and `constants` are global to the dump; functions
  reference constants by `index`.
- `ts_erasures` records every TypeScript erasure / lowering performed
  by the compiler per ADR-0002 §4. The array is empty for `.js`
  inputs.

### 2.2 `Function`

```json
{
  "id": <u32>,
  "name": "<string>",
  "span": [<startOffset>, <endOffset>],
  "loc":  { "start": [<line>, <col>], "end": [<line>, <col>] },
  "registers": { "locals": <u16>, "scratch": <u16> },
  "upvalues":   <u16>,
  "feedback":   <u16>,
  "code":  [ <Instruction>, ... ],
  "spans": [ <SpanEntry>, ... ]
}
```

- `id` is unique within the dump and stable across runs of the same
  source.
- `span` is byte offsets into the **original** source (post-erasure
  for surviving TS nodes; the dropped nodes have entries in
  `ts_erasures` instead).
- `loc` is computed line/col, 1-based, UTF-16 code-unit columns to
  match `Span` semantics from `oxc_span`.
- `registers.locals` is the number of declared locals;
  `registers.scratch` is scratch slots. Total register count is the
  sum.
- `code` is sorted by `pc`.
- `spans` is sorted by `pc`. Multiple `pc` values may share a span.

### 2.3 `Instruction`

```json
{
  "pc": <u32>,
  "op": "<MNEMONIC>",
  "operands": [
    { "name": "dst",   "kind": "register",       "value": 0 },
    { "name": "lhs",   "kind": "register",       "value": 0 },
    { "name": "rhs",   "kind": "register",       "value": 1 },
    { "name": "feedback", "kind": "feedback_slot", "value": 0 }
  ]
}
```

`kind` is one of: `"register"`, `"const_index"`, `"branch_offset"`,
`"feedback_slot"`, `"arg_count"`, `"upvalue_index"`,
`"function_id"`, `"immediate_i8"`, `"immediate_i32"`,
`"immediate_double"`. The set grows as new opcodes need new operand
kinds; adding a new `kind` is **not** a version bump.

### 2.4 `SpanEntry`

```json
{ "pc": <u32>, "span": [<startOffset>, <endOffset>] }
```

### 2.5 `Constant`

```json
{ "index": <u32>, "kind": "<kind>", "value": <typed value> }
```

`kind` and the corresponding `value` shapes:

| Kind | `value` shape | Example |
| --- | --- | --- |
| `"string"` | object `{ "utf16": [<u16>, ...] }` | `{"utf16":[97,98,99]}` |
| `"number"` | JSON number | `1.5` |
| `"bigint"` | object `{ "bigint": "<dec>" }` | `{"bigint":"42"}` |
| `"regexp"` | object `{ "pattern": "<utf8>", "flags": "<utf8>" }` | — |
| `"function_id"` | JSON number (function index) | `3` |

Strings encode as `{ "utf16": [...] }` to round-trip lone surrogates
exactly. Numeric constants use IEEE-754 doubles; `NaN`, `Infinity`,
and `-Infinity` encode as JSON strings (`"NaN"`, `"Infinity"`,
`"-Infinity"`) since vanilla JSON has no representation for them.

### 2.6 `ErasureEntry`

```json
{
  "kind": "TSAsExpression",
  "action": "replace",
  "span": [<startOffset>, <endOffset>],
  "note": "as cast erased"
}
```

- `kind` matches the OXC AST node name (ADR-0002 §4 tables).
- `action` ∈ `"drop"`, `"replace"`, `"lower"`.
- `span` is the original source span of the erased / lowered node.
- `note` is a short human-readable hint.

### 2.7 Determinism rules

- All field names are `snake_case` except for the top-level
  `otterBytecodeDumpVersion` (camelCase to match the trace schema and
  to match the legacy convention from
  `AGENTS.md` debugging section).
- Functions sorted by `id` ascending; instructions and spans sorted
  by `pc` ascending; constants sorted by `index` ascending.
- Object key order in pretty-printed output: as defined in the
  schema above (e.g., `id, name, span, loc, registers, upvalues,
  feedback, code, spans`).
- Pretty printing uses 2-space indentation with a trailing newline.
  CI normalizers reuse `serde_json::to_string_pretty` with the
  documented field order.

## 3. Instruction / lifecycle trace

`otter --trace [<file>] [--trace-file <out>] [--trace-filter <re>]`
emits a Chrome Trace Event JSON file. This format opens in Chrome
DevTools Performance and in Perfetto unmodified, matching the
existing project convention (see legacy `AGENTS.md` debugging
section).

### 3.1 Top-level shape

```json
{
  "otterTraceSchemaVersion": 1,
  "displayTimeUnit": "ns",
  "traceEvents": [ <Event>, ... ]
}
```

### 3.2 Event kinds

#### `vm.instruction` (phase `i`, instant)

Emitted only when `--trace` is on. Args:

```json
{
  "name": "vm.instruction",
  "ph": "i",
  "ts": <u64>,
  "pid": 1,
  "tid": 1,
  "args": {
    "pc": <u32>,
    "op": "<MNEMONIC>",
    "fn": <function_id>,
    "module": "<specifier>",
    "span": [<startOffset>, <endOffset>]
  }
}
```

#### `vm.call` / `vm.return` (phases `B` / `E`)

Bracket events around each call frame:

```json
{
  "name": "vm.call",
  "ph": "B",
  "ts": <u64>,
  "pid": 1,
  "tid": 1,
  "args": {
    "fn": <function_id>,
    "module": "<specifier>",
    "argc": <u16>
  }
}
```

`vm.return` mirrors with `"ph": "E"`.

#### `vm.gc` (phases `B` / `E`)

```json
{
  "name": "vm.gc",
  "ph": "B",
  "ts": <u64>,
  "pid": 1,
  "tid": 1,
  "args": {
    "live_bytes": <u64>,
    "total_bytes": <u64>
  }
}
```

GC integration arrives later in foundation; the schema is reserved.

#### `vm.compile` (phases `B` / `E`)

```json
{
  "name": "vm.compile",
  "ph": "B",
  "ts": <u64>,
  "pid": 1,
  "tid": 1,
  "args": {
    "function_id": <u32>,
    "bytecode_size": <u32>
  }
}
```

#### `vm.diagnostic` (phase `i`)

Emitted whenever a `Diagnostic` is produced (compile-time or
runtime). Args:

```json
{
  "name": "vm.diagnostic",
  "ph": "i",
  "ts": <u64>,
  "pid": 1,
  "tid": 1,
  "args": {
    "kind": "<DiagnosticKind>",
    "code": "<CODE>",
    "module": "<specifier>",
    "span": [<startOffset>, <endOffset>]
  }
}
```

### 3.3 Rules

- **Timestamps** (`ts`) are monotonic ns since runtime start. Never
  wall-clock.
- **Filtering.** `--trace-filter <regex>` filters by `args.module`,
  `args.fn` (when names are available), or `args.code`. Filtering by
  mnemonic alone is intentionally disallowed because traces are
  meant to be navigated by location, not by opcode.
- **Default file.** Trace output defaults to `otter-trace.json` in
  the current directory. A `.txt` extension on `--trace-file` falls
  back to a text rendering for ad-hoc viewing; the JSON file remains
  the canonical form.
- **Cost.** `--trace` is **off by default**. Filters apply before
  encoding so a filter that rejects most events keeps overhead near
  zero.
- **No Otter-only primary trace format.** When a standard format
  exists (Chrome Trace Event), use it. This rule mirrors the legacy
  `AGENTS.md` debug-tooling roadmap and continues into the new
  engine.

## 4. Snapshot testing

- Disassembly text is consumed by `otter test` golden files. Tests
  pin the **format version** (`v1`), not the engine version.
- JSON dumps used in tests are pretty-printed with 2-space
  indentation, fields in the documented order. The runner provides
  a normalizer.
- Trace files are not consumed in golden tests directly (they
  contain timestamps); tests assert on event counts and on filtered
  subsets.

## 5. CLI controls

| Flag | Purpose |
| --- | --- |
| `--dump-bytecode <file>` | Print disassembly text to stdout. |
| `--dump-bytecode=json <file>` | Print JSON dump to stdout. |
| `--dump-bytecode-file <out>` | (Reserved; first added by task `07`.) Redirect dump output to a file. |
| `--trace [<file>] [--trace-file <out>] [--trace-filter <re>]` | Capture instruction trace. Default file `otter-trace.json`. |
| `--trace-events <comma-separated>` | (Reserved; future amendment.) Restrict event kinds. |

These flags are owned by ADR-0003 §4. This spec only defines what
the resulting files look like.

## 6. Versioning

| Format | Version field | Initial |
| --- | --- | --- |
| Disassembly text | banner `v1` token | `v1` |
| JSON dump | `otterBytecodeDumpVersion` | `1` |
| Trace JSON | `otterTraceSchemaVersion` | `1` |

A bump is **required** when any of the following change:

- a field is renamed or removed;
- an operand encoding changes;
- a mnemonic is renamed;
- an existing event category, phase, or `args` field changes;
- a constant `kind` value's payload shape changes;
- a TS erasure `action` value's semantics change.

Adding a new optional field, a new operand `kind`, a new constant
`kind`, a new event category, or a new TS erasure `kind` is **not**
a bump.

Each bump is recorded in `CHANGELOG.md` next to this spec (the file
is created lazily — the first bump creates it). Bumps are also
mentioned in the relevant slice's status block.

## 7. Snapshot rules and pretty-printing helper

- Slice tasks that touch the dump (`07`, `09`, `10`, `11`, `12`,
  `13`) commit golden files alongside fixtures.
- The runner's normalizer pretty-prints JSON with 2-space indent,
  documented field order, sorted maps where the schema requires
  sorting, and trailing newline.
- The text dump's golden helper does not pretty-print; it diffs
  byte-for-byte.

## 8. Non-goals

- **No** stable on-disk bytecode cache format. (Separate spec, far
  later.)
- **No** profiler protocol. CPU profiles continue to use the
  Chrome / V8 `*.cpuprofile` schema documented for the legacy
  binary.
- **No** debugger wire protocol (CDP). Foundation is too early for
  CDP.
- **No** heap snapshot schema. (Separate spec, post-foundation.)

## Spec amendments

(Empty — no amendments yet.)

When a slice changes the disassembly, JSON dump, or trace contract,
append a dated entry of the form:

```markdown
### 20YY-MM-DD — <short title>

- **Change:** <what was added / removed / changed>
- **Reason:** <why>
- **Linked task:** [task XX](../tasks/XX-...)
```

## References

- ADR-0001: [`../adr/0001-staging-directory.md`](../adr/0001-staging-directory.md)
- ADR-0002: [`../adr/0002-oxc-frontend.md`](../adr/0002-oxc-frontend.md)
- ADR-0003: [`../adr/0003-public-api-and-cli.md`](../adr/0003-public-api-and-cli.md)
- Test harness spec: [`./otter-test-harness.md`](./otter-test-harness.md)
- Foundation plan §M2 / §"VM and Interpreter Design Bar".
- Task: [`../tasks/06-spec-bytecode-dump-disasm-trace.md`](../tasks/06-spec-bytecode-dump-disasm-trace.md)
