# Contributing Overview

Otter's active engine and product crates live under `crates/`.

Start with:

- [Architecture](../engine/architecture.md) for active crate ownership;
- [Frontend And Compilation](../engine/frontend.md) before changing
  parser, module detection, or TypeScript behavior;
- [`ES_CONFORMANCE.md`](../../../../ES_CONFORMANCE.md) before changing
  ECMAScript behavior;
- [`AGENTS.md`](../../../../AGENTS.md) for repository-specific workflow
  rules.

This book is the contributor-facing source of truth for stable workflows.
Task files are implementation plans and closeout history; when a workflow
stabilizes, document how to use it here.

## Choosing A Crate

- GC storage, tracing, handles, weak/finalization, heap stats, snapshots,
  and external-memory accounting belong in `crates/otter-gc`.
- Value representation, object model, bytecode execution, intrinsics,
  native callable dispatch, and source/compiler integration belong in
  `crates/otter-vm`.
- Public embedding, capabilities, event-loop handles, worker/isolate
  runners, and host-operation scheduling belong in
  `crates/otter-runtime`.
- CLI behavior belongs in `crates/otter-cli`.
- New Web/API/module/product crates belong under `crates/*`.

Do not introduce a parallel runtime stack or copied compatibility modules.

## Working Rules

- Keep changes vertical and reviewable.
- Prefer improving active APIs over preserving unsafe,
  slow, startup-heavy, or confusing compatibility shims.
- Do not add thread-local heap lookup or context-free GC access.
- Keep `unsafe` code inside `otter-gc`. Other active crates keep
  `#![forbid(unsafe_code)]`; audited VM adapters may call doc-hidden raw
  GC APIs but must not make them contributor-facing.
- Update runtime behavior, TypeScript declarations, docs, and tests
  together when a public surface changes.
- High-level APIs are welcome only when they compile down to the same
  runtime shape as handwritten code. Add benchmarks when changing native
  dispatch, bootstrap, or startup.
- Use AST tooling such as `oxc`/SWC for JS/TS parsing or transforms.
  Do not regex-parse JavaScript or TypeScript.
- Preserve deterministic output order where observable. Prefer ordered
  maps or explicit sorting for JSON, object-key snapshots, tests, and
  iterator output.

## Common Commands

```bash
cargo build
cargo test --all --all-features
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
mdbook build docs/book
```

Fast loops:

```bash
cargo test -p otter-gc
cargo test -p otter-vm
cargo test -p otter-runtime
```

For feature work covered by Test262, establish a targeted baseline first,
fix by failure category, then record the before/after pass rate in the
task closeout or PR notes. If a change significantly changes conformance,
regenerate the conformance report through the repository-approved Test262
commands before closing the task.

## Tests And Examples

Match test depth to risk:

- narrow helper behavior: focused unit or integration tests;
- GC/worker/session misuse: compile-fail tests;
- public JS-visible behavior: engine fixtures and targeted Test262;
- contributor APIs: rustdoc examples or book-backed integration tests.

Book examples for active APIs should either compile through normal cargo gates
or point at the exact test file that backs them. Design-only snippets are
shown as `ignore`.

## Closing Work

Close implementation work only after its validation gates are actually
green. Record what shipped, note command output or blockers, and keep the
book aligned with any workflow that changed.
