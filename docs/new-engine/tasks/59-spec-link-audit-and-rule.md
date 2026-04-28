# Task 59 — ECMA-262 spec-link audit + repo rule

## Goal

Make ECMA-262 spec links (https://tc39.es/ecma262/) a **mandatory
part of every module-level and function-level docstring** in the
new engine that implements a spec algorithm or surface, and
back-fill the audit on already-shipped code.

Today the codebase has spec links in some places (e.g.
`promise_dispatch.rs` mentions §27.2.5.4; `unwind_throw` cites
§27.7.5.3) but the coverage is patchy. A reader looking at, say,
`array_prototype.rs` cannot tell which TC39 algorithm a given
function implements without searching the spec by hand.

The spec is the source of truth; the rule makes traceability
effortless.

## Scope — rule

Add an explicit clause to ADR-0001 §6 (the docstring rules) and
to `docs/new-engine/tasks/README.md` "Working rules" §6:

> **Spec-link rule.** Any module / function in `crates-next/*`
> that implements an ECMA-262 algorithm, intrinsic, or
> spec-mandated semantic MUST cite the spec section in its
> `///` (or `//!`) docstring with a deep link of the form
> `https://tc39.es/ecma262/#sec-<anchor>`. The link goes in the
> module's `# See also` block and in the function's `# Algorithm`
> or `# See also` block (whichever fits — short helpers can use a
> single-line `/// Spec: <url>`). Non-spec helpers (parser glue,
> compiler internals, dispatch plumbing) are exempt.

When a slice introduces a new spec-faithful surface, the docstring
gains the link in the same commit.

## Scope — audit

Walk every public function in `crates-next/otter-vm/`,
`crates-next/otter-runtime/`, `crates-next/otter-compiler/`,
`crates-next/otter-bytecode/`. For each that implements a spec
surface, add the matching `https://tc39.es/ecma262/#sec-<anchor>`
link.

Priority order (depth of spec content):

1. `string_prototype.rs` — every method maps to a §22.1.3.x
   subsection.
2. `array_prototype.rs` — §23.1.3.x.
3. `regexp_prototype.rs` — §22.2.6.x.
4. `number/` — §21.1.3.x + ToNumber / ToUint32 / ToInt32 (§7.1.x).
5. `bigint/` — §21.2.3.x + spec arithmetic.
6. `json/` — §25.5.1 / §25.5.2.
7. `math/` — §21.3.2.x.
8. `promise.rs` + `promise_dispatch.rs` — §27.2 (partly done).
9. `intrinsics.rs` — depends on the intrinsic.
10. `microtask.rs` — §9.4.x (HostEnqueuePromiseJob etc.).
11. `lib.rs` (VM) — §10 (Execution Contexts), §13.x for opcodes
    with spec mappings.
12. `compiler/lib.rs` — only the spec-faithful pieces (Annex B,
    AssignmentExpression desugaring, `ToNumber` coercion,
    iterator protocol, etc.).

Each entry expands to a single-commit edit per file (or per a
small group of files in the same module) so review stays
manageable.

## Out of scope

- Linking from comments inside function bodies (only docstrings).
- Linking from tests.
- Re-checking spec compliance — this is a documentation pass,
  not a correctness audit. Bugs found along the way are filed as
  separate tasks.

## Files / directories you may touch

- `docs/new-engine/adr/0001-staging-directory.md` — append the
  spec-link rule.
- `docs/new-engine/tasks/README.md` — add the rule to "Working
  rules".
- Every file listed in the priority order above.

## Acceptance criteria

- ADR-0001 §6 carries the spec-link rule.
- The README's "Working rules" §6 is updated.
- Every public function in the audited files that implements a
  spec algorithm carries a TC39 deep link.
- A grep for `tc39.es/ecma262` returns hits in every audited
  file.
- Engine suite green (no behaviour change).

## Risks

- Some helpers straddle "implements spec X" vs "internal glue"
  — when in doubt, add the link. Over-linking is harmless;
  under-linking is the actual problem.
- ECMA-262 anchor IDs sometimes change between living editions;
  prefer the stable tc39.es URLs over numbered editions.

## Status

- not started
