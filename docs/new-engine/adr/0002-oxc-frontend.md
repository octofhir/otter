# ADR-0002 — OXC-only JS/TS Frontend

- **Status:** accepted
- **Date:** 2026-04-26
- **Deciders:** project lead
- **Related:**
  - [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
    §"Architecture Rules" #17 ("OXC owns parsing"), §"Foundation Shape",
    §"M2: TypeScript Frontend and Minimal VM Harness"
  - [`docs/new-engine/adr/0001-staging-directory.md`](./0001-staging-directory.md)
  - [task 03](../tasks/03-adr-oxc-frontend.md)

## Context

The new engine in `crates-next/*` needs a JavaScript and TypeScript
frontend for parsing, source spans, AST traversal, and TypeScript
type-only syntax stripping. The foundation plan locks the choice:
[OXC](https://github.com/oxc-project/oxc) is mandatory; no custom
parser, no regex parsing, no parallel parser stack.

OXC is a Rust-native toolchain for JavaScript / TypeScript with
production-grade speed, complete TS syntax coverage, and a stable AST
shape. It is the only choice that fits the foundation plan's
"interpreter-first, TypeScript-first, OXC-owns-parsing" rules
simultaneously.

This ADR locks:

- the exact OXC crates that the new engine depends on;
- the version pinning policy;
- the rules for using OXC across staging crates;
- the TypeScript erasure rules for the foundation subset;
- the explicitly rejected alternatives.

## Decision

### 1. OXC crates and versions (pinned 2026-04-26)

The new engine consumes the following OXC crates at the exact pinned
versions below. These are the latest stable releases from crates.io as
of 2026-04-26 (verified via `https://crates.io/api/v1/crates/<crate>`).

| Crate | Version | Purpose |
| --- | --- | --- |
| `oxc_allocator` | `0.127` | Bump-allocated arena that owns AST nodes. |
| `oxc_span` | `0.127` | `Span`, `SourceType`, source-position primitives. |
| `oxc_syntax` | `0.127` | Syntactic enums shared across OXC crates. |
| `oxc_ast` | `0.127` | JS / TS AST node definitions. |
| `oxc_parser` | `0.127` | Recursive-descent parser for JS / TS. |
| `oxc_ast_visit` | `0.127` | `Visit` / `VisitMut` traversal traits. |
| `oxc_diagnostics` | `0.127` | Diagnostic reporting (`miette`-compatible). |
| `oxc_resolver` | `11.19` | Module specifier resolver (used post-M1). |
| `oxc_codegen` | `0.127` | Pretty-printer used **only** for code-frame output and `otter check` text (never for executing transformed code). |

Notes:

- The `0.127` release was published on 2026-04-20 across the OXC
  family. `oxc_resolver` is on a different release cadence (`11.19.1`,
  published 2026-02-28).
- `oxc_semantic` is **not** added in this ADR. The new engine emits
  bytecode directly from the OXC AST and does not rely on OXC's
  semantic analyzer for foundation work. If a slice needs semantic
  data, that slice amends this ADR.
- `oxc_transformer` is **not** added in this ADR. TypeScript type-only
  syntax stripping is implemented in the new engine's compiler crate
  (rule 4 below). If a future slice needs the OXC transformer, that
  slice amends this ADR with a justification.

### 2. Version pinning policy

- The OXC crates are declared in the root `Cargo.toml` under
  `[workspace.dependencies]` with caret pins (e.g., `oxc_parser =
  "0.127"`). All `crates-next/*` crates that need OXC pull from the
  workspace declaration via `oxc_parser.workspace = true`.
- A version bump is a deliberate, single-commit task with its own task
  file. The bump commit:
  - updates every OXC crate in `[workspace.dependencies]` together so
    the family stays in sync (mixing `oxc_parser = 0.128` with
    `oxc_ast = 0.127` is a compile error and is forbidden anyway);
  - re-runs the full staging test suite and the conformance subset for
    every implemented slice;
  - records before/after pass counts in the bump's status block.
- Auto-bumps from `cargo update` are disallowed in CI for the OXC
  family. The lockfile is committed; CI fails on lockfile drift for
  these crates without an accompanying ADR amendment.
- `Cargo.toml` carries an explicit comment marking the OXC entries as
  "ADR-0002-managed" so future contributors know not to bump them
  casually.

### 3. Use rules

- **Parsing.** All JS / TS parsing in `crates-next/*` goes through
  `oxc_parser::Parser`. There is no other parser in the build graph.
- **AST.** The new engine walks the OXC AST directly. There is no
  shadow AST, no "lite" copy of OXC types, and no second AST format
  on the new path.
- **Spans.** Source spans on bytecode, in diagnostics, and in stack
  traces are `oxc_span::Span` values stored as `(start: u32, end: u32)`
  byte offsets into the original source. `Span` is **never** rebuilt
  from a transformed source string.
- **Allocator.** AST lifetime is tied to an `oxc_allocator::Allocator`
  arena owned by the parse step. Compilation reads the AST inside the
  arena's lifetime and emits bytecode (`'static` data) before the
  arena is dropped.
- **Diagnostics.** Parse and early errors are produced by OXC and
  surface as `Diagnostic` values per ADR-0003 (CLI / API). The CLI
  formatter reuses `oxc_diagnostics` (`miette`-style code frames) for
  rendering, never re-implementing snippet extraction.
- **Resolver.** Module specifier resolution uses `oxc_resolver`. There
  is no parallel resolver. Foundation slices `07`–`13` execute single-
  file scripts and may not need the resolver yet; later slices wire it
  in via the public API's `ModuleLoader` trait.
- **Codegen.** `oxc_codegen` is allowed only for diagnostic / `otter
  check` text output. Executing transformed code is **forbidden**: the
  compiler emits bytecode from the AST directly, never round-tripping
  through textual JS.

### 4. TypeScript erasure rules (foundation subset)

The new compiler crate (`crates-next/otter-compiler`) implements a
type-only erasure pass over the OXC AST before lowering to bytecode.
The rules below are exhaustive for the foundation subset (M2 plus
slices `08`–`13`). Each rule names the OXC AST node it acts on so an
engineer can audit the pass without reading code.

#### Erased (drop entirely or replace with operand)

| OXC AST node | Action |
| --- | --- |
| `TSTypeAnnotation` | drop |
| `TSTypeAliasDeclaration` | drop |
| `TSInterfaceDeclaration` | drop |
| `TSDeclareFunction` | drop |
| `TSModuleDeclaration` with `declare` | drop |
| `ImportDeclaration` with `import_kind = type` | drop |
| `ExportNamedDeclaration` with `export_kind = type` | drop |
| `ExportSpecifier` with `export_kind = type` | drop the specifier |
| `ImportSpecifier` with `import_kind = type` | drop the specifier |
| `TSAsExpression` | replace with `expression` |
| `TSSatisfiesExpression` | replace with `expression` |
| `TSNonNullExpression` | replace with `expression` |
| `TSInstantiationExpression` | replace with `expression` |
| `TSTypeAssertion` (legacy `<T>x`) | replace with `expression` |
| `TSAbstractMethodDefinition` | drop |
| `TSEmptyBodyFunctionExpression` | drop |
| Method / function `type_parameters` | drop |
| Method / function `return_type` | drop |
| Class field `type_annotation` | drop |
| Class field with `declare = true` | drop |
| Method `accessibility`, `optional`, `definite`, `override` modifiers | drop (no runtime effect) |
| Parameter `type_annotation` | drop |
| Parameter `optional = true` | drop the flag (default `undefined` already) |
| Parameter property modifiers (`public`, `private`, `protected`, `readonly` on constructor params) | **lowered**, not erased — see "Lowered" below |

#### Lowered (kept in runtime form)

| OXC AST node | Action |
| --- | --- |
| Constructor parameter property | lowered to a class field declaration plus a `this.<name> = <name>` assignment at the start of the constructor body |

#### Diagnosed (rejected at compile time on the new path)

The following TypeScript constructs are rejected with structured
`TS_UNSUPPORTED` diagnostics until a dedicated future slice opts in.
This list is intentional and conservative.

| OXC AST node | Reason for deferral |
| --- | --- |
| `TSEnumDeclaration` | needs runtime emission; M9+ |
| `TSModuleDeclaration` (non-`declare`, with runtime members) | needs runtime emission; M9+ |
| `Decorator` (anywhere) | proposal moves; revisit later |
| `JSXElement` / `JSXFragment` / etc. | not in foundation |

#### Rules

- The erasure pass is implemented as `oxc_ast_visit::VisitMut` (or an
  equivalent owned-AST rewrite within the OXC arena). It does **not**
  re-emit JS source and re-parse. Surviving nodes keep their original
  `Span`s.
- Each erased / lowered node leaves a single audit-trail entry the
  compiler can dump on `--dump-bytecode=json` (the bytecode dump
  schema reserves a `ts_erasures` array for this).
- Rule additions or changes require an ADR amendment (a new dated
  entry at the bottom of this ADR). Quietly adding a new erased node
  is not allowed.

### 5. Forbidden alternatives

The following are explicitly rejected and may not be added to the
build graph:

- **SWC (`swc_*` crates).** Mature alternative parser, but having two
  parsers in two stacks is the exact failure mode this ADR exists to
  prevent. SWC is also a different AST shape; sharing diagnostics
  across two ASTs is a re-implementation tax.
- **Biome (`biome_js_parser`, `biome_js_syntax`).** Same reasoning as
  SWC. Acceptable parser, wrong choice for foundation because it
  forces a second AST.
- **Esprima / Acorn / any JS-runtime-based parser.** Foundation is
  pure Rust; importing a JS parser into Rust is a regression.
- **Hand-written recursive-descent or PEG parser for JS / TS.** OXC's
  parser is faster than anything we would write and is maintained by
  a dedicated team; rebuilding it is wasted work.
- **Regex-based scanners that approximate JS / TS syntax.** Forbidden
  for any purpose, including identifier scanning, string-literal
  extraction, import path rewriting, comment stripping, and "just
  detect whether a file has TypeScript syntax". The repo `AGENTS.md`
  rule already says this; this ADR makes it a build-graph constraint.
- **Tree-sitter grammars.** Useful for editor tooling, not for
  authoritative parsing on the runtime path. Foundation does not use
  it.

If a future slice has a use case that genuinely needs one of the
above (for example, an editor-side highlighter built on tree-sitter
that ships separately from the runtime), that slice amends this ADR
explicitly and explains why OXC cannot do the job.

### 6. CI / lint enforcement

This ADR records intent. The actual enforcement is added by task
`07`'s repo grep / `cargo deny` configuration:

- `cargo deny` rule rejects every crate listed in §5 from the
  workspace dependency graph.
- A small grep CI check rejects `regex` usage anywhere inside
  `crates-next/otter-syntax`, `crates-next/otter-compiler`, and
  any crate whose role is to consume the AST. (`regex` is allowed in
  test fixtures and in CLI argument matching, where it is not used as
  a parser.)
- Cargo lockfile drift on OXC crates fails CI without a paired ADR
  amendment.

### 7. Coexistence with legacy `crates/*`

The legacy `crates/*` directories are reference-only per ADR-0001.
They have their own (older) OXC pin (`0.123`) frozen in their
`Cargo.toml` files. That is irrelevant to the new engine because the
legacy directories are not in the build graph. This ADR does **not**
attempt to keep version parity with `crates/*`; the legacy code stays
as-is until the end-of-foundation cleanup commit deletes it.

## Consequences

For the build graph:

- Root `Cargo.toml` carries the OXC versions in
  `[workspace.dependencies]` (added by task `07` when the first
  staging crate is created — this ADR commits the policy, not the
  declarations).
- New `crates-next/*` crates that parse, walk, or print JS / TS
  declare `oxc_parser.workspace = true` etc. They never re-pin a
  different version locally.

For contributors:

- No alternative parser slips in via "just for one helper".
- An OXC version bump is a tracked task with conformance
  re-measurement, not a `cargo update`.
- TypeScript erasure rule changes are reviewed.

For TypeScript users:

- `enum`, `namespace` (with runtime members), and decorators are
  rejected with clear diagnostics until a slice adds them
  intentionally. This is a documented foundation limitation, not a
  bug. Each slice that lifts a restriction adds an ADR amendment.

## Alternatives considered

See §5 above. Each rejected alternative carries a one-line rationale
in the table.

## ADR amendments

(Empty — no amendments yet.)

When a slice needs to change an OXC pin or an erasure rule, append a
dated entry of the form:

```markdown
### 20YY-MM-DD — <short title>

- **Change:** <what was added / removed / changed>
- **Reason:** <why>
- **Linked task:** [task XX](../tasks/XX-...)
```

Do not edit the original §1–§5 tables — append to the amendment log
so the history of decisions stays visible.

## References

- OXC repository: <https://github.com/oxc-project/oxc>
- OXC crates on crates.io: <https://crates.io/teams/github:oxc-project:publishers>
- Foundation plan: [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md)
- Staging-directory ADR: [`0001-staging-directory.md`](./0001-staging-directory.md)
- Task: [`docs/new-engine/tasks/03-adr-oxc-frontend.md`](../tasks/03-adr-oxc-frontend.md)
