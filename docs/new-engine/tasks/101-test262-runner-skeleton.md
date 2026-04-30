# Task 101 — Test262 runner skeleton

**Parent:** [100 — Test262 conformance](./100-test262-conformance.md).
Read the parent first; this task only ships the bare skeleton.

## Scope

Boot a fresh `crates-next/otter-test262` workspace member that
walks the `vendor/test262` corpus and reports a total file count.
No metadata parsing, no execution, no reports — that lands in
slices 102 / 103 / 104.

## Why

Every later slice needs the crate to exist and the submodule to
be wired. Decoupling the boilerplate keeps the metadata / runner /
report PRs small.

## Deliverables

1. **Workspace member** at `crates-next/otter-test262/`:
   - `Cargo.toml` listing the dependencies enumerated in
     [task 100](./100-test262-conformance.md#crate-skeleton-target-shape--implemented-in-slice-101)
     (`otter-runtime`, `otter-compiler`, `otter-bytecode`,
     `walkdir`, `ignore`, `serde`, `serde_yaml`, `serde_json`,
     `rayon` *or* `tokio` (pick one — document the choice),
     `indicatif`, `anyhow`, `thiserror`, `clap`).
   - `src/main.rs` — clap CLI with two subcommands stubbed:
     `run` and `diff`. `run` accepts
     `--filter <glob>` / `--shard N/M` / `--timeout` /
     `--max-heap-bytes` / `--output` / `--dry-run`; only
     `--dry-run` is implemented in this slice.
   - `src/lib.rs` — re-exports of the (empty) `harness`,
     `metadata`, `runner`, `report` modules so 102 / 103 / 104
     can fill them.
2. **Submodule** `vendor/test262` pinned to a commit. Update
   `.gitmodules` and the workspace `README.md` with the
   `git submodule update --init --remote` instruction.
3. **Traversal stub** in `src/runner.rs` that walks
   `vendor/test262/test/` recursively, counts `.js` files
   excluding `_FIXTURE.js`, and prints `total: N`. Use `ignore`
   so `.gitignore` patterns inside the corpus are honoured.
4. **Refusal-to-launch** check: the runner prints an actionable
   error message and exits non-zero if `vendor/test262/` is
   absent or empty.
5. **`justfile`** entry: `just test262-dry` running the dry-run
   path with no args.
6. **Crate `README.md`** under `crates-next/otter-test262/`
   summarising the goal, the safety rules from task 100 §Safety
   controls, and a quick-start sequence.

## Files to touch

- `crates-next/otter-test262/Cargo.toml` (new)
- `crates-next/otter-test262/README.md` (new)
- `crates-next/otter-test262/src/{main,lib,runner,harness,metadata,report}.rs` (new)
- `Cargo.toml` (workspace `members` array — add the new crate)
- `.gitmodules` (new submodule)
- `vendor/test262` (submodule)
- `justfile` (new target)
- `docs/new-engine/tasks/README.md` (cross-link)

## Sequencing notes

- Land before slice 102. Slice 102 imports `metadata::Frontmatter`
  from this skeleton.
- Don't add any logic beyond walkdir traversal and CLI parsing.
  Anything that feels like real work belongs in 102 / 103 / 104.
- Pick `rayon` over `tokio` unless the existing `otter-runtime`
  forces async — the test262 driver is CPU-bound per worker; the
  per-test isolation rules out shared async runtime state.

## Gates

- `cargo build -p otter-test262` clean.
- `cargo clippy --workspace --all-targets -- -D warnings` clean.
- `cargo run -p otter-test262 -- run --dry-run` prints a non-zero
  total without executing any test.
- `cargo run -p otter-test262 -- run --dry-run` exits with
  actionable error when `vendor/test262/` is empty.
- `git status` shows the new files + the submodule pin.

## Spec links

- Crate-layout convention (parent task §"Crate skeleton"):
  [task 100](./100-test262-conformance.md#crate-skeleton-target-shape--implemented-in-slice-101)
- ADR-0001 spec-link rule (mandatory in every public docstring):
  [`docs/new-engine/adr/0001-design-discipline.md`](../adr/0001-design-discipline.md)
