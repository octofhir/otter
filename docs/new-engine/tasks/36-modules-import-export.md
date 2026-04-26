# Task 36 — ES modules: `import` / `export`

## Goal

Load and execute ES modules with `import` / `export` declarations,
dynamic `import()`, and `import.meta`.

## Scope

- A `ModuleLoader` trait already lives on `RuntimeBuilder` — flesh
  it out with a default `file://` resolver that handles relative
  specifiers.
- Compile-time: `import { x } from "./other.ts"` records the
  binding; `export { y }` / `export default` record outgoing
  bindings. The compiler tracks per-module bindings separately
  from the script-level ones.
- Module evaluation order: resolve the dependency graph
  iteratively (no recursion), execute leaf modules first.
- Live bindings: `import { counter } from "..."` reflects later
  mutations in the exporting module.
- `import.meta.url` returns the resolving specifier string.
- `import("...")` returns a Promise (depends on task 34).

## Out of scope

- HTTP-based imports.
- Top-level `await`.
- Import maps / package.json resolution.

## Files / directories you may touch

- `crates-next/otter-runtime/` (loader plumbing).
- `crates-next/otter-vm/` (live-binding cells).
- `crates-next/otter-compiler/`
- `tests/engine/modules/`

## Acceptance criteria

- A two-module fixture imports a named export and reads it.
- Cyclic imports detect at evaluation time with a clear
  `RangeError` (foundation rule: hard cap on resolution depth).
- `import("./x.ts")` returns a settled promise whose value has the
  module's namespace.
- Engine suite green.

## Verification commands

```bash
cargo run -p otter-cli -- test --suite engine --filter modules/
```

## Risks

- Live bindings require an indirection cell (`Rc<RefCell<Value>>`
  or similar) — coordinate with the closure / upvalue model from
  task 22 so we do not duplicate primitives.

## Status

- not started
