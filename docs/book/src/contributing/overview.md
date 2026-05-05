# Contributing Overview

Otter's active new engine lives under `crates-next/`. The legacy crates
under `crates/` are reference-only unless a task explicitly says
otherwise.

Start with:

- `docs/new-engine/repository-map.md` for crate ownership;
- `docs/new-engine/tasks/70-gc-master-tracker.md` for GC/runtime work;
- `docs/new-engine/adr/` for accepted architecture decisions;
- `AGENTS.md` for repository-specific workflow rules.

## Working Rules

- Keep changes vertical and reviewable.
- Prefer breaking interim `crates-next/*` APIs over preserving unsafe or
  confusing compatibility shims.
- Do not add thread-local heap lookup or context-free GC access.
- Keep unsafe code inside `otter-gc` or narrow audited VM adapters.
- Update runtime behavior, TypeScript declarations, docs, and tests
  together when a public surface changes.

## Common Commands

```bash
cargo build
cargo test --all --all-features
cargo fmt --all
cargo clippy --all-targets --all-features -- -D warnings
```
