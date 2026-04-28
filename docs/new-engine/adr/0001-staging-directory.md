# ADR-0001 — Staging Directory for the New Foundation Engine

- **Status:** accepted
- **Date:** 2026-04-26
- **Deciders:** project lead
- **Related:** [`NEW_ENGINE_FOUNDATION_PLAN.md`](../../../NEW_ENGINE_FOUNDATION_PLAN.md),
  [`docs/new-engine/repository-map.md`](../repository-map.md),
  [task 02](../tasks/02-staging-directory-decision.md)

## Context

The foundation plan (`NEW_ENGINE_FOUNDATION_PLAN.md`) requires a clean
build graph for the new engine. The legacy stack under `crates/*`
(`otter-vm`, `otter-runtime`, `otter-jit`, `otterjs`, `otter-modules`,
`otter-web`, `otter-pm*`, `otter-test262`, `otter-nodejs`,
`otter-node-compat`, plus path-only `otter-macros` and `otter-profiler`)
is large, has parked compatibility shims, has active → parked
dependency edges (e.g., `otterjs → otter-nodejs`), and would push back
on every cleanup attempt during foundation work.

The plan permits a temporary staging directory so the new core can stay
clean while the old code is kept on disk for reference. The user has
chosen the strongest form of that rule:

- The new engine lives in **`crates-next/`** and is written from
  scratch.
- The legacy `crates/*` directories are **removed from the workspace
  build graph entirely**. They are not built, tested, formatted, or
  linted by any new-foundation command. We are not patching them, not
  fixing them, not migrating them in place, not porting code out of
  them, and not moving them under `crates-next/`. They are frozen
  reference, deleted in a single cleanup commit at the end of
  foundation.

## Decision

1. **Staging directory:** `crates-next/`.
2. **Crate-name prefix:** `otter-*` — the canonical names. The legacy
   `crates/*` directories also used this prefix, but they have been
   excluded from the workspace by this ADR, so there is no collision
   inside the build graph. Examples of staging crates: `otter-syntax`,
   `otter-bytecode`, `otter-compiler`, `otter-vm`, `otter-runtime`,
   `otter-cli`, `otter-test`. (No `otter-next-*` prefix is used; the
   user explicitly chose to drop the `-next-` infix because the legacy
   crates are not workspace members.)
3. **Workspace:**
   - Root `Cargo.toml` `[workspace] members` lists **only**
     `crates-next/*` crates as they are created.
   - Root `Cargo.toml` carries `[workspace] exclude = ["crates/*"]` so
     a future stray path-dep cannot accidentally re-enter the legacy
     code into the build graph.
   - `[workspace.dependencies]` and `[profile.*]` are kept (cleaned of
     legacy oxc pins); ADR-0002 adds the pinned OXC versions for the
     new foundation.
4. **Crate metadata:** every `crates-next/*` crate sets
   `publish = false` and `edition = "2024"` (or the latest stable
   edition at creation time).
5. **Unsafe boundary:** every `crates-next/*` crate declares
   `#![forbid(unsafe_code)]`. Foundation phase does not introduce a new
   GC or JIT, so no exception is needed. If a future slice introduces
   `crates-next/otter-gc`, that slice amends this ADR explicitly.
6. **LLM-friendly module documentation (mandatory).** Every Rust
   module file in every `crates-next/*` crate begins with a `//!`
   crate- or module-level doc comment that an autonomous coding agent
   can scan to understand what the file holds without reading the
   bodies. The shape is fixed:
   ```rust
   //! <one-sentence summary of what this module is responsible for>
   //!
   //! # Contents
   //! - `<TypeOrFn>` — <one-line purpose>
   //! - `<TypeOrFn>` — <one-line purpose>
   //!
   //! # Invariants
   //! - <single-sentence invariant the file enforces, if any>
   //!
   //! # See also
   //! - [`crate::<other_module>`] — <why a reader might jump there>
   //! - <link to the relevant task / ADR / spec under
   //!   `docs/new-engine/`>
   ```
   The "Contents" list is not exhaustive — list the public items and
   any private item that is non-obvious. The "Invariants" and "See
   also" sections may be omitted if the module truly has none, but
   "Summary" and "Contents" are required.
   Public types, traits, and functions also carry their own `///` doc
   comments; this point governs the file-level docstring specifically.
   The CI clippy gate runs `-D missing_docs` for every
   `crates-next/*` crate so this rule is enforced mechanically, not
   socially.

   **Function-level LLM-friendly comments.** Every public function
   carries a `///` rustdoc; that's the `-D missing_docs` baseline.
   On top of that, **non-trivial functions** (anything more than a
   one-line wrapper) must also carry a structured doc comment that
   a coding agent can scan without reading the body. The shape:
   ```rust
   /// <one-sentence summary of what the function does and why>
   ///
   /// # Algorithm
   /// <numbered or bulleted steps; reference spec sections / task
   /// numbers where relevant>
   ///
   /// # Invariants
   /// - <pre/post-conditions; what the caller can rely on>
   ///
   /// # Errors
   /// - `<ErrorVariant>` — <when this is returned>
   ///
   /// # See also
   /// - [`<related_fn>`] — <why a reader might jump there>
   ```
   Sections are optional — keep only the ones that carry weight.
   "Algorithm" is required for any function whose body is more
   than ~15 lines or that implements a spec algorithm; for small
   helpers the one-sentence summary is enough. Private helpers
   that are non-obvious (cycle handling, state-machine steps,
   spec-faithful reaction jobs) get the same treatment even though
   `missing_docs` doesn't enforce it. The bar: a coding agent
   modifying a sibling function should be able to understand this
   one from its docstring alone.

   **ECMA-262 spec-link rule (mandatory).** Any module or
   function in `crates-next/*` that implements an ECMA-262
   algorithm, intrinsic, or spec-mandated semantic MUST cite the
   spec section in its docstring with a deep link of the form
   `https://tc39.es/ecma262/#sec-<anchor>`. The link goes in the
   module's `# See also` block and in the function's `# Algorithm`
   or `# See also` block — short helpers may use a single-line
   `/// Spec: <url>`. When more than one section is in play
   (e.g., an algorithm calls into a spec abstract operation),
   list each. Non-spec helpers (parser glue, compiler internals,
   dispatch plumbing) are exempt from the link rule but still
   carry the regular docstring. Audit + back-fill on already-
   shipped code is filed as
   [task 59](../tasks/59-spec-link-audit-and-rule.md); from this
   ADR amendment onward, every new spec-faithful surface
   includes the link in the same commit it lands.
7. **Import rule:**
   - `crates-next/*` crates depend on each other and on third-party
     crates from crates.io.
   - `crates-next/*` crates **must not** add a path dependency or any
     other reference to any crate under `crates/*`. Even temporarily.
     Even "just for one helper". The `[workspace] exclude` entry plus
     this rule are the two defenses.
   - Old `crates/*` crates may depend on whatever they already depend
     on; we do not touch them.
8. **No migration of legacy code.** Nothing inside `crates/*` is
   ported, ported back, or rewritten in place. The new engine is
   written from scratch in `crates-next/*`. Legacy `crates/*` directories
   are kept on disk as frozen reference until the project decides to
   delete them (in a dedicated cleanup commit, separate from any
   foundation slice). They are never built, tested, linted, or
   modified during the foundation phase.
9. **End-of-foundation cleanup.** Once the new engine in
   `crates-next/*` is feature-complete enough to replace the legacy
   stack as the shipped product, a single dedicated cleanup commit:
   - deletes the legacy `crates/*` directories outright (no archive,
     no rename — they have served their purpose as reference);
   - removes the `[workspace] exclude = ["crates/*"]` entry from the
     root `Cargo.toml`;
   - changes nothing else.
   `crates-next/` itself stays. There is no "promotion move" of
   directories — the new engine simply lives at `crates-next/*` until
   the legacy code is gone, at which point the staging name has
   served its purpose. A later, optional, also dedicated cleanup
   commit may rename `crates-next/` to `crates/` for cosmetic reasons,
   but that is purely a directory rename with no semantic change and
   is not required by this ADR.
10. **Abort rule.** If a `crates-next/*` crate is abandoned, it is
    removed in a dedicated cleanup commit. No crate may import an
    abandoned `crates-next/*` crate.
11. **Exit signal.** The foundation phase is over once the new engine
    in `crates-next/*` ships as the project's primary runtime and
    the legacy `crates/*` directories have been deleted.

## Consequences

For contributors:

- The supported developer workflow uses `cargo build`, `cargo test`,
  `cargo clippy`, `cargo fmt` from the workspace root. After this ADR,
  those commands cover **only** the staging crates. Until task `07`
  lands the first staging crate, the workspace has zero members and
  the workspace-level commands are no-ops.
- The legacy `otter` binary built from `crates/otterjs` is no longer
  built by the workspace. If a developer needs to run it for
  reference, they invoke `cargo build --manifest-path crates/otterjs/Cargo.toml`
  explicitly. That is not a supported foundation-phase command.
- AGENTS.md, CLAUDE.md, and the foundation plan refer to a number of
  legacy commands (`just test`, `just lint`, `cargo run -p otterjs`,
  etc.). These continue to work for the legacy stack only. Task `04`
  (ADR-0003) freezes the new CLI shape and replaces these in the new
  binary as it is built.

For CI:

- CI configuration that runs `cargo …` at the workspace root will
  produce empty results until the first staging crate exists. CI
  changes are part of task `07`.
- CI gates on legacy crates are turned off as soon as this ADR
  lands. If those gates were ratchets, that ratchet does not migrate;
  the new foundation establishes its own ratchets in the slice tasks.

For the build graph:

- Immediately after this ADR, `cargo metadata --format-version=1
  --offline` **fails** with `the manifest is virtual, and the
  workspace has no members`. That is intentional and transient: task
  `07` adds the first staging crate and `cargo metadata` starts
  succeeding from that point on. Until then, every workspace-level
  cargo command is a no-op (or errors with the same message).
- A future `path = "../crates/<anything>"` reference inside
  `crates-next/*` is rejected at `cargo` resolution time by the
  `[workspace] exclude` entry combined with this ADR's import rule.

## Alternatives considered

- **`engine-next/`.** Rejected: less obvious next to the existing
  `crates/`. The directory name should hint at what it contains.
- **`crates-foundation/`.** Rejected: longer and more ambiguous. The
  staging directory is not "foundation-only"; it is the future of
  every active crate.
- **`crates/_next/`.** Rejected: nesting under `crates/` makes the
  legacy / staging boundary harder to spot in `ls` and tooling.
- **Keep legacy crates in `members` and gate them behind features.**
  Rejected by the user: every minute spent keeping legacy compileable
  is a minute not spent on the new engine, and the legacy code has
  active → parked dependency edges that would force engineering work
  to fix before any new slice is acceptable.

## References

- Foundation plan §"Staging Directory" and §"Repository Cleanup
  Policy".
- Repository map: [`docs/new-engine/repository-map.md`](../repository-map.md).
- Task: [`docs/new-engine/tasks/02-staging-directory-decision.md`](../tasks/02-staging-directory-decision.md).
