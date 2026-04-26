# Task 03 — ADR-0002: OXC-only JS/TS Frontend

## Goal

Write `docs/new-engine/adr/0002-oxc-frontend.md`. This ADR makes
[OXC](https://github.com/oxc-project/oxc) the **only** JavaScript and
TypeScript frontend on the new foundation path. It forbids hand-written
parsers, regex parsing, and parallel parser stacks.

## Scope

The ADR must answer all of:

- **Decision.** OXC owns:
  - tokenization
  - JS / TS parsing (`oxc_parser`)
  - AST representation (`oxc_ast`)
  - source spans (`oxc_span`)
  - traversal / visit (`oxc_ast_visit`)
  - syntactic-only diagnostics (parse errors, early errors that OXC reports)
  - TypeScript type-only syntax stripping/lowering (via OXC AST + a small
    erasure pass owned by the new compiler crate)
- **Consequences.**
  - The new compiler emits Otter bytecode by walking the OXC AST. No second
    AST format on the new path.
  - Source spans on bytecode come directly from OXC `Span` values. They are
    preserved end-to-end into diagnostics, stack traces, and trace events.
  - `oxc_resolver` is the default module specifier resolver for the new
    runtime; no parallel resolver stack.
  - `oxc_codegen` may be used for diagnostic code frames and for
    `otter check` output, but **not** for executing transformed code — the
    compiler emits bytecode directly from the AST.
- **Forbidden alternatives.**
  - SWC, Biome, Esprima, hand-written recursive-descent parsers, regex-based
    transformations, string scanners that approximate JS/TS syntax. Document
    each as explicitly rejected.
  - Mixing parsers across crates (e.g., one crate using `swc_ecma_parser` for
    "speed" and another using OXC) is forbidden, even temporarily.
- **TypeScript erasure rules.**
  - Erased: type annotations on parameters, return types, variable
    declarations, class fields; `interface`, `type` aliases, `declare`
    statements, `import type`/`export type`, `as` expressions, satisfies,
    non-null assertion `!`, type-only generic syntax that can be erased
    safely.
  - Compiled (lowered to runtime form): `enum`, `namespace` (only when it
    has runtime members), parameter property syntax in classes,
    constructor field declarations.
  - Diagnosed (rejected at compile time on the new path): `decorators` until
    a separate slice opts in; experimental TS syntax not in
    `tc39/proposal-type-annotations`.
  - Each rule cites the OXC AST node it acts on so a future engineer can
    audit the erasure pass without reading code.
- **Migration / coexistence rule.**
  - The active runtime stack (`crates/otter-vm`, `crates/otter-runtime`,
    `crates/otterjs`) currently uses `oxc` as well; this ADR does not allow
    that to silently become "two parsers in two stacks". On the new path
    the parser version is pinned in the staging workspace's `Cargo.toml` and
    is allowed to differ from the active stack only during the staging
    period. Document the version-pinning rule.
- **CI / lint enforcement.**
  - Add (in a follow-up task, not this one) a `cargo deny` rule or an
    explicit workspace lint that rejects dependencies on alternative JS
    parsers. The ADR records the *intent* and names the deny entries.

## Out of scope

- Adding `oxc_*` crates to any `Cargo.toml`. (Task `08` and beyond add them
  per slice.)
- Implementing the TypeScript erasure pass. (Task `08` does this.)
- Writing the `cargo deny` configuration. (A later cleanup task does.)

## Files / directories you may touch

- Create: `docs/new-engine/adr/0002-oxc-frontend.md`
- Read-only: everything else

## Acceptance criteria

- ADR file exists with the standard ADR sections (Status, Context, Decision,
  Consequences, Alternatives, References).
- The "Forbidden alternatives" section names every parser ruled out and gives
  one sentence of rationale per item.
- The "TypeScript erasure rules" section is exhaustive for the foundation
  subset (M2 + M3–M7 surface). It is acceptable to mark slices later than
  M7 as "to be revisited per ADR amendment".
- The ADR cites OXC crate names and version policy (pin major + minor in the
  staging workspace; bump deliberately, not via `cargo update`).
- The ADR is linked from `NEW_ENGINE_FOUNDATION_PLAN.md` is **not** required —
  do not edit the plan from this task. Linking from
  `docs/new-engine/tasks/README.md` (already done) is sufficient.

## Verification commands

```bash
test -f docs/new-engine/adr/0002-oxc-frontend.md
rg -n "oxc_parser|oxc_ast|oxc_span|oxc_ast_visit" \
    docs/new-engine/adr/0002-oxc-frontend.md
rg -n "swc|biome|esprima|regex.*parse" \
    docs/new-engine/adr/0002-oxc-frontend.md   # must be present as "rejected"
```

## Risks

- **"Just for one helper" regex.** Every regex-as-parser temptation must be
  rejected up front, including identifier scanning, string-literal
  extraction, and import path rewriting. Spell these out in the ADR.
- **Parser version drift** between active and staging crates causes hard-to-
  debug behavior differences. The version-pinning rule prevents this.
- **OXC API churn.** OXC is fast-moving. The ADR must say: pin a known-good
  version, batch upgrades, and treat each upgrade as a tracked task with a
  conformance re-run.

## Next task

Proceed to [`04-adr-public-api-cli-shape.md`](./04-adr-public-api-cli-shape.md).

## Status

- **done**
- last update: 2026-04-26
- artifacts: [`docs/new-engine/adr/0002-oxc-frontend.md`](../adr/0002-oxc-frontend.md)
- verification:
  - ADR exists at `docs/new-engine/adr/0002-oxc-frontend.md`.
  - `rg -n "oxc_parser|oxc_ast|oxc_span|oxc_ast_visit"` — matches lines
    46/48/49/50 and others; all four crate names are mentioned with
    their pinned versions.
  - `rg -in "swc|biome|esprima|tree-sitter|regex"` — every rejected
    parser appears in §"Forbidden alternatives" with a rationale.
- decisions locked:
  - OXC version pins as of 2026-04-26 verified against
    `crates.io/api/v1/crates/<crate>`:
    - `oxc_allocator`, `oxc_span`, `oxc_syntax`, `oxc_ast`,
      `oxc_parser`, `oxc_ast_visit`, `oxc_diagnostics`,
      `oxc_codegen` — `0.127` (published 2026-04-20);
    - `oxc_resolver` — `11.19` (published 2026-02-28).
  - `oxc_semantic` and `oxc_transformer` are **not** added; require an
    ADR amendment if a future slice needs them.
  - TypeScript erasure rules for the foundation subset are exhaustive
    and listed by OXC AST node name.
  - Rejected alternatives: SWC, Biome, Esprima/Acorn/any JS-runtime
    parser, hand-written parsers, regex scanners, tree-sitter.
  - Enforcement plan deferred to task `07` (cargo deny rule + grep CI
    check + Cargo.lock drift check).
