# Task 08 — TypeScript Frontend Skeleton

## Goal

Land the TypeScript frontend skeleton end-to-end on the staging stack:
`.ts` input parses through OXC, type-only syntax is stripped/lowered per
ADR-0002, source spans survive into bytecode and diagnostics, and the same
public API and CLI commands run `.ts` scripts as first-class input.

This task does **not** add new runtime semantics beyond what task `07`
already supports. It widens the **frontend** to TypeScript.

## Scope

### Frontend (`crates-next/otter-syntax`)

- Add explicit parser entry points:
  - `parse_javascript(source, span_offset)` — `oxc_parser` in JS mode.
  - `parse_typescript(source, span_offset)` — `oxc_parser` with `SourceType`
    set to `.ts`/`.mts`/`.cts` based on the file extension or explicit
    `SourceInput` constructor.
- Detect source kind from:
  - `SourceInput::from_path` — file extension (`.js`, `.mjs`, `.cjs`,
    `.ts`, `.mts`, `.cts`).
  - `SourceInput::from_javascript` / `SourceInput::from_typescript` —
    explicit constructors.
- Emit structured `Diagnostic`s for parse errors with the OXC `Span`
  preserved as `(start_offset, end_offset)` plus computed `line`/`col`.
- Provide a `code_frame(source, span, context_lines = 2)` helper used by
  the CLI formatter (lives in `otter-runtime` once it exists; this
  task moves the helper from a stub to a real implementation).

### Erasure pass (`crates-next/otter-syntax::erase`)

Implement the foundation subset of TypeScript erasure rules from ADR-0002.
This task implements **only** rules necessary for tasks `09`–`13` to
parse the same fixtures with `.ts` extensions, plus the rules ADR-0002
calls "erased":

- type annotations on parameters, returns, variable declarations,
  class fields
- `interface` and `type` aliases — drop entirely (no runtime emission)
- `declare` statements — drop entirely
- `import type` / `export type` — drop entirely
- `as` expressions — replace with the inner expression
- `satisfies` expressions — replace with the inner expression
- non-null assertion `!` — replace with the operand
- type-only generic syntax that can be erased safely (no `enum`, no
  `namespace` with runtime members; both reject at compile time with a
  clear diagnostic for now)

The erasure pass is implemented as an OXC AST visitor; it produces a
post-erasure AST that the compiler consumes. The post-erasure AST keeps
all surviving `Span`s pointing into the **original** TypeScript source,
not a synthesized JS source — diagnostics must reference the user's
file, not a transformed one.

### Diagnostics

- Unsupported TypeScript syntax (e.g., `enum`, `namespace`, decorators)
  produces a structured `Diagnostic` with:
  - `kind = "syntax"`, code `"TS_UNSUPPORTED"`,
  - the original span,
  - a one-line explanation,
  - a "this slice does not implement X yet" hint where appropriate.
- The CLI renders the diagnostic with code-frame text using the helper
  above. Golden tests cover the rendering.

### Compiler integration (`crates-next/otter-compiler`)

- The compiler already walks the OXC AST after task `07`. This task wires
  the erasure pass between parsing and compilation: parsing returns the
  raw OXC AST, the erasure pass returns the surviving subset, and the
  compiler walks that.
- Source spans on bytecode (per task `06`'s spec) come from the post-
  erasure nodes' original `Span`.

### Public API / CLI

- `SourceInput::from_path` accepts `.ts`, `.mts`, `.cts` and routes them
  through `parse_typescript`.
- `otter run script.ts` executes through the same pipeline as `script.js`.
- `otter check script.ts` runs parse + erase + compile and reports
  diagnostics without executing.
- `otter --dump-bytecode script.ts` works.

### Tests

- Engine fixtures (added under `tests/engine/typescript/`):
  - `parses-empty.ts`
  - `erases-type-annotation.ts` — `let x: number = undefined;` runs
  - `erases-interface.ts` — `interface I {} undefined;` runs
  - `erases-as-expression.ts` — `(undefined as any);` runs
  - `rejects-enum.ts` — `enum E {}` produces a `TS_UNSUPPORTED` diagnostic
- Each fixture has the appropriate `expect` block in its metadata
  (`exit_code`, `throws`, etc.).
- Disassembly golden files prove the erasure does not change emitted
  bytecode for the supported subset.
- Unit tests in `otter-syntax::erase` covering each erasure rule with
  a small AST snippet.

## Out of scope

- Type checking (no `tsc`, no `tsgo`, no constraint solving). Erasure
  only.
- `enum`, `namespace`, decorators, parameter properties — rejected with
  diagnostics. A later task amends ADR-0002 and adds them.
- JSX. Not in foundation.
- Project-aware path mapping. Not in foundation.
- Type-aware ICs or specialization. Not in foundation.

## Files / directories you may touch

- Edit / create under `crates-next/otter-syntax/`
- Edit / create under `crates-next/otter-compiler/`
- Edit / create under `crates-next/otter-runtime/` (CLI formatter only)
- Edit / create under `crates-next/otter-cli/`
- Create fixtures under `tests/engine/typescript/`

You **must not** read-and-copy from `crates/*` either: legacy code is
reference for understanding constraints only. Every line under
`crates-next/*` is written from scratch (ADR-0001 §8). And of course
do not modify any `crates/*` file.

## Acceptance criteria

- `otter run tests/engine/typescript/erases-type-annotation.ts` exits 0.
- `otter check tests/engine/typescript/rejects-enum.ts` exits non-zero
  with a diagnostic containing the `TS_UNSUPPORTED` code and a code frame
  pointing at the `enum` keyword.
- `otter test --suite engine` runs all `tests/engine/typescript/*.ts`
  fixtures with the recorded outcomes.
- Disassembly for `erases-type-annotation.ts` matches the disassembly
  for an equivalent `.js` file (golden files prove the erasure is
  bytecode-neutral).
- Source spans in diagnostics for a `.ts` file point into the **original**
  `.ts` text, not a transformed JS string.
- `cargo clippy --workspace --all-targets --all-features -- -D warnings`
  passes.
- No regex parsing of JS or TS introduced anywhere.
- ADR-0002 is amended only if a foundation rule changed during
  implementation; otherwise the ADR is unchanged.

## Verification commands

```bash
cargo run -p otter-cli -- run \
    tests/engine/typescript/erases-type-annotation.ts
cargo run -p otter-cli -- check \
    tests/engine/typescript/rejects-enum.ts ; test $? -ne 0
cargo run -p otter-cli -- test --suite engine
rg -n 'regex|Regex' crates-next/otter-syntax/src \
    crates-next/otter-compiler/src | grep -v '// .*spec.*allowed' \
    && exit 1 || true   # no regex parsing leak
```

## Risks

- **Spans get clobbered** when AST nodes are removed/replaced. Erasure must
  preserve original offsets; do not rebuild source.
- **Hidden re-parsing.** Some erasure-style passes re-emit JS and re-parse.
  Forbidden — walk the OXC AST in place.
- **Diagnostic regressions.** A syntax error in a `.ts` file must point at
  the `.ts` file. Add a regression test.
- **Erasure scope creep.** Resist landing `enum`, `namespace`, decorators
  here. Each is its own slice with its own ADR amendment.

## Next task

Proceed to [`09-string-core-slice.md`](./09-string-core-slice.md).

## Status

- not started
- last update: —
- artifacts: TS frontend module, erasure pass, `tests/engine/typescript/`
  fixtures
