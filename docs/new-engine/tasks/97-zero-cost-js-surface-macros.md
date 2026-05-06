# Task 97 — Zero-cost JS surface macros over static specs

## Status

- [x] open only after task 96 lands and at least two surfaces use the builder/spec backend
- [x] active `crates-next/otter-macros` or equivalent proc-macro crate added
- [x] macros generate static specs, not runtime registries
- [x] macro expansion audit documented with `cargo expand` output or equivalent
- [x] macro-generated surfaces benchmark equal to handwritten specs
- [x] mdBook macro guide updated with generated-shape examples
- [x] gates green

## Progress Notes

- 2026-05-06: first macro slice added `crates-next/otter-macros` with
  `#[js_namespace(name = "...", spec = SPEC_IDENT)]`.
- The macro consumes `#[js_fn(name = "...", length = N)]` on inline module
  functions, rejects duplicate exported function names in one namespace, and
  emits `NamespaceSpec` / `MethodSpec` records with `NativeCall::Static`.
- The first slice was intentionally namespace-only; the follow-up slice added
  class and grouped namespace declarations. Benchmark comparison and final
  contributor recipes remain open before task 97 can close.
- 2026-05-06: expanded the macro set to `#[js_class]` and `raft!`.
  `#[js_class]` emits `ClassSpec` with constructor, constructor/static-side
  methods, prototype instance methods, and prototype accessors. Instance
  methods use `#[js_method]`; JavaScript static methods use
  `#[js_static_method]`. Both compile to `NativeCall::Static` by default.
- 2026-05-06: `cargo expand` audit captured in
  [`97-expansion-audit.md`](./97-expansion-audit.md).
- 2026-05-06: validation commands passed for the current macro slice:
  `cargo fmt --all`, `cargo test -p otter-macros -p otter-vm`,
  `cargo clippy --workspace --all-targets --all-features -- -D warnings`,
  `cargo test --workspace`, `mdbook build docs/book`, and fff static scans
  for thread-default GC lookup, hot-path boxed registries, and async
  VM/GC/context capture patterns.
- 2026-05-06: added `crates-next/otter-macros/benches/js_surface_macros.rs`
  and captured handwritten-vs-macro parity in
  [`97-benchmark-report.md`](./97-benchmark-report.md). `#[js_namespace]`,
  `raft!`, and `#[js_class]` macro-generated surfaces were not slower than
  equivalent handwritten specs on the same Task 96 builder path.

## Goal

Add high-level macros for contributors without hiding runtime control flow
or adding overhead. Macros are syntax sugar over task 96's static specs and
builders; they are not a second registration system.

Breaking changes are allowed. Macro ergonomics matter, but production
runtime shape wins over preserving an early macro signature.

## Source

- [`96-production-js-surface-builders.md`](./96-production-js-surface-builders.md)
  static specs and mutator-bound builders.
- [`95-contributor-book-and-extension-guides.md`](./95-contributor-book-and-extension-guides.md)
  mdBook documentation requirements.
- Existing `intrinsics!` macro in `crates-next/otter-vm` is the local
  precedent: metadata is compile-time-visible and lookup is direct.

## Scope

### 97.1 — Macro crate

Add an active macro crate under `crates-next/` and wire it into the
workspace only after task 96's backend is stable enough to generate
against.

The crate must not depend on legacy `crates/*` code. It may depend on
standard proc-macro parsing/generation crates, but generated code must use
only active `crates-next` APIs.

### 97.2 — Initial macro set

Start with the smallest macro set that removes repetitive boilerplate:

- `#[js_namespace]` for namespace objects;
- `#[js_class]` for constructor/prototype/static method surfaces;
- `raft!` or an equivalent declarative grouped-spec macro for modules that
  do not need proc-macro attributes.

Defer until the backend and docs are mature:

- async native binding sugar;
- host-owned object surface macros;
- hosted-module loader macros;
- GC trace derive macros.

### 97.3 — Generated shape contract

Macro expansion must generate static specs plus normal Rust functions,
roughly:

```rust
static MATH_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Math",
    methods: &[
        MethodSpec {
            name: "abs",
            length: 1,
            attrs: Attr::builtin_function(),
            call: NativeCall::Static(math_abs),
        },
    ],
    ..
};
```

It must not generate:

- per-call allocations;
- runtime string parsing of metadata;
- dynamic dispatch for static builtins;
- hidden global mutation outside the centralized bootstrap registry;
- hidden permission checks or async host-op scheduling;
- captures of `RuntimeCx`, `NativeCtx`, `Value`, `Frame`, `Gc<T>`, or
  `Local<'gc, T>` across `.await`.

### 97.4 — Diagnostics and compile-time checks

Macros should reject bad API declarations at compile time when possible:

- missing JS name;
- duplicate method/accessor name in one surface;
- invalid arity metadata;
- accessor declared with data-property flags;
- async macro used on a sync-only native signature;
- hidden / inferred exported names where explicit names are required.

### 97.5 — Migration slice

Migrate only the surfaces already covered by task 96 first. Compare
handwritten spec output and macro-generated output before expanding the
migration.

No broad Web API / Node API macro migration until:

- macro-generated startup is benchmarked;
- generated code is documented in the book;
- reviewers can inspect generated shape easily.

## Performance requirements

- Macro-generated static builtin call overhead must match handwritten
  task-96 specs within benchmark noise.
- Macro-generated bootstrap must not increase cold `RuntimeBuilder::build()`
  time beyond an explicitly approved budget.
- Compile time may increase, but runtime and startup regressions are not
  acceptable without a production justification.

## Out of scope

- Designing a stable out-of-tree plugin ABI.
- Replacing task 96 builders; macros call into them.
- Hiding capability enforcement, bootstrap order, or async scheduling.

## Validation gates

- [ ] `cargo test --workspace` green.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] `cargo expand` or equivalent expansion snapshots are checked or
  documented for representative macro uses.
- [ ] Benchmark report compares handwritten vs macro-generated surfaces.
- [ ] mdBook macro page explains when to use macros and when manual code is
  required.

## Closing

Update mdBook first, then close this task. The book is the contributor API
surface; task files are implementation history.
