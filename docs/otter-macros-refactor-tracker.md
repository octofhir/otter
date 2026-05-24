# Otter Macros Refactor — Progress Tracker

Living state for the Phase 4 macro rewrite. Updated at the end of
every session so the next one resumes without re-scanning the
codebase.

Design reference: [`docs/otter-macros-design.md`](otter-macros-design.md).
Plan entry: Task 4.1 / 4.2 / 4.3 in
[`docs/architecture-refactor-plan-2026-05.md`](architecture-refactor-plan-2026-05.md).

## Status snapshot

| Sub-phase | Scope                                                          | Status      |
| --------- | -------------------------------------------------------------- | ----------- |
| 4.1a      | New macros land in `otter-macros` (no production callers yet)  | In progress |
| 4.1b      | Delete `js_namespace` / `js_class` / `raft` helper attrs       | Pending     |
| 4.1c      | mdbook chapter: naming theme + per-macro examples              | Pending     |
| 4.2a      | Port **JSON** (pathfinder, smallest namespace)                 | Pending     |
| 4.2b      | Port Math / Reflect / Atomics / Console in parallel            | Pending     |
| 4.2c      | Port Proxy / Date / Iterator (first `couch!` users)            | Pending     |
| 4.2d      | Port the rest (collections, weak refs, typed arrays, …)        | Pending     |
| 4.3       | Rewrite `otter-modules` (`otter:ffi`, `otter:kv`, `otter:sql`) | Pending     |
|           | + `otter-web` if `burrow!` / `lodge!` apply                    |             |

## Macro implementation checklist (4.1a)

| Macro            | File                                        | Tests                   | State            |
| ---------------- | ------------------------------------------- | ----------------------- | ---------------- |
| `holt!`          | `crates/otter-macros/src/holt.rs`           | `tests/holt.rs`         | Skeleton shipped |
| `couch!`         | `crates/otter-macros/src/couch.rs`          | `tests/couch_*.rs`      | Pending          |
| `raft!` (extend) | `crates/otter-macros/src/raft.rs`           | `tests/raft.rs`         | Existing         |
| `#[dive]` attr   | `crates/otter-macros/src/dive.rs`           | `tests/dive_*.rs`       | Pending          |
| `burrow!`        | `crates/otter-macros/src/burrow.rs`         | `tests/burrow_*.rs`     | Deferred (Q3)    |
| `lodge!`         | `crates/otter-macros/src/lodge.rs`          | `tests/lodge_*.rs`      | Pending (4.3)    |
| `Pelt` derive    | `crates/otter-macros/src/derive_pelt.rs`    | `tests/derive_pelt.rs`  | Pending          |
| `Groom` derive   | `crates/otter-macros/src/derive_groom.rs`   | `tests/derive_groom.rs` | Pending          |

Per-macro notes (referenced from the table above):

- `holt!` skeleton shipped 2026-05-24 — covers `name` / `feature` /
  `methods` fields plus derived `<NAME>_SPEC` + `Intrinsic` ident
  defaults. Still pending: `constants = [...]` and `accessors = [...]`
  field support, trybuild matrix (duplicate name, missing name,
  unknown field), `attrs` per-row override inside the methods block.

## Production consumer inventory

Files we walk during 4.2 / 4.3. Each one becomes a "DONE" row once
its hand-written installer is replaced by the matching macro
callsite and Test262 deltas land in the port commit message.

### Vanilla JS intrinsics → `holt!` / `couch!`

| Surface             | Source                                                     | Target macro       | Port state |
| ------------------- | ---------------------------------------------------------- | ------------------ | ---------- |
| JSON                | `crates/otter-vm/src/json/mod.rs`                          | `holt!`            | Pending (4.2a pathfinder) |
| Math                | `crates/otter-vm/src/math/mod.rs`                          | `holt!`            | Pending    |
| Reflect             | `crates/otter-vm/src/reflect.rs`                           | `holt!`            | Pending    |
| Atomics             | `crates/otter-vm/src/atomics.rs`                           | `holt!`            | Pending    |
| Console             | `crates/otter-vm/src/console.rs`                           | `holt!`            | Pending    |
| Object              | `crates/otter-vm/src/object_statics.rs` + `intrinsics/object.rs` | `holt!` + `couch!` (`Object.prototype`) | Pending |
| Function            | `crates/otter-vm/src/function_prototype.rs` + `intrinsics/function.rs` | `couch!` | Pending |
| Array               | `crates/otter-vm/src/array_prototype.rs` + `array_statics.rs` + `intrinsics/array.rs` | `couch!` | Pending |
| String              | `crates/otter-vm/src/string/{intrinsic,prototype,statics}.rs` | `couch!`         | Pending    |
| Number              | `crates/otter-vm/src/number/prototype.rs` + `intrinsics/number.rs` | `couch!`    | Pending    |
| Boolean             | `crates/otter-vm/src/boolean/{intrinsic,mod,prototype}.rs` | `couch!`           | Pending    |
| Symbol              | `crates/otter-vm/src/intrinsics/symbol.rs`                 | `couch!` + `holt!` (Symbol namespace) | Pending |
| Date                | `crates/otter-vm/src/date/prototype.rs` + `intrinsics/date.rs` | `couch!`       | Pending    |
| Proxy               | `crates/otter-vm/src/intrinsics/proxy.rs`                  | `couch!`           | Pending    |
| Iterator            | `crates/otter-vm/src/intrinsics/iterator.rs`               | `couch!` + `holt!` | Pending    |
| Promise             | `crates/otter-vm/src/bootstrap_promise.rs`                 | `couch!`           | Pending    |
| RegExp              | `crates/otter-vm/src/bootstrap_regexp.rs`                  | `couch!`           | Pending    |
| BigInt              | `crates/otter-vm/src/bootstrap_bigint.rs`                  | `couch!`           | Pending    |
| Map / Set / WeakMap / WeakSet | `crates/otter-vm/src/bootstrap_collections.rs`    | `couch!` (×4)      | Pending    |
| WeakRef / FinalizationRegistry | `crates/otter-vm/src/bootstrap_weak_refs.rs`     | `couch!` (×2)      | Pending    |
| ArrayBuffer / SharedArrayBuffer | `crates/otter-vm/src/bootstrap_array_buffer.rs` | `couch!` (×2)      | Pending    |
| DataView            | `crates/otter-vm/src/bootstrap_data_view.rs`               | `couch!`           | Pending    |
| TypedArray family   | `crates/otter-vm/src/bootstrap_typed_array.rs`             | `couch!` (×N + `%TypedArray%`) | Pending |
| Temporal classes    | `crates/otter-vm/src/temporal/intrinsic.rs`                | `couch!` (×5) + `holt!` (Now) | Pending |
| Timers              | `crates/otter-vm/src/timers.rs`                            | `holt!` (or `#[dive]` on globalThis) | Pending |

### Otter-specific modules → `lodge!`

| Module     | Source                                  | Target macro | Port state |
| ---------- | --------------------------------------- | ------------ | ---------- |
| `otter:ffi`| `crates/otter-modules/src/ffi.rs`       | `lodge!`     | Pending (4.3) |
| `otter:kv` | `crates/otter-modules/src/kv.rs`        | `lodge!`     | Pending (4.3) |
| `otter:sql`| `crates/otter-modules/src/sql.rs`       | `lodge!`     | Pending (4.3) |

### Web APIs → mix

| Surface    | Source                                                   | Target macro | Port state |
| ---------- | -------------------------------------------------------- | ------------ | ---------- |
| URL        | `crates/otter-web/src/url.rs`                            | `couch!`     | Pending    |
| Blob       | `crates/otter-web/src/blob.rs`                           | `couch!`     | Pending    |
| Headers    | `crates/otter-web/src/headers.rs`                        | `couch!`     | Pending    |
| Request / Response | `crates/otter-web/src/request_response.rs`       | `couch!` (×2)| Pending    |

## Per-session log

Most recent session first. One-line "what landed + what's next"
per entry. New entries go at the top.

### 2026-05-24 — `holt!` skeleton + docs + tracker

- `docs/otter-macros-design.md` written, naming theme approved by
  owner (holt / couch / Pelt / Groom + keep raft / burrow / lodge /
  dive). Q1 / Q4 default answers picked (theme as-written; derives
  in 4.1). Q2 hard-cutover sequencing approved (a / b / c separate
  PRs).
- This tracker added at `docs/otter-macros-refactor-tracker.md`.
- `crates/otter-macros/src/lib.rs` module docstring rewritten with
  the full naming-theme table + per-macro examples.
- `docs/book/src/macros/overview.md` rewritten with the same theme
  plus expanded per-macro narrative.
- `crates/otter-macros/src/holt.rs` shipped: parses `name` /
  `feature` / `methods` (plus optional `spec` / `intrinsic` ident
  overrides); emits `<NAME>_SPEC: NamespaceSpec`, `pub struct
  Intrinsic;`, and the matching `BuiltinIntrinsic` impl with an
  `install` body that calls `NamespaceBuilder::from_spec_with_value_roots`
  plus `bootstrap::define_global_value`. Promoted
  `bootstrap::define_global_value` from `pub(crate)` → `pub` so
  macro consumers reach it through the documented re-export path.
  Integration test at `crates/otter-macros/tests/holt.rs` checks
  the generated spec + `BuiltinIntrinsic` metadata.
- Next: extend `holt!` with `constants` and `accessors` fields,
  add trybuild compile-fail matrix (duplicate / missing / unknown
  field), then start `couch!` (class intrinsic). JSON picked as
  4.2a pathfinder afterward.

## Acceptance ratchet

- Each 4.2 / 4.3 port commit message records the Test262 delta for
  the touched suite.
- `MAX_DEFAULT_GC_ALLOCATIONS` in `crates/otter-vm/src/bootstrap.rs`
  must stay ≥ the actual count after each port; bump in the same
  PR when needed, with a one-line justification.
- Workspace `cargo test --all --all-features` + `cargo clippy
  --all-targets --all-features -- -D warnings` green per PR.
- `forbid(unsafe_code)` on `otter-vm` / `otter-runtime` /
  `otter-compiler` / `otter-bytecode` stays load-bearing — any
  macro that needs `unsafe` for the expansion is a design bug.
