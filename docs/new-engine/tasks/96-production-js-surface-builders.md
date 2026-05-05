# Task 96 — Production JS surface specs, builders, and bootstrap

## Status

- [ ] open after GC Phase 1 closeout (tasks 76A, 77-84) and GC bench gates (task 91) are usable
- [ ] `Attr` / property-attribute helpers added
- [ ] `PropertySpec`, `MethodSpec`, `AccessorSpec`, `ConstructorSpec`, `ClassSpec`, and `NamespaceSpec` added
- [ ] `ObjectBuilder`, `FunctionBuilder`, `ConstructorBuilder`, `ClassBuilder`, and `NamespaceBuilder` added
- [ ] builtin install path is centralized through specs/builders
- [ ] native builtin fast path avoids boxed closure dispatch where possible
- [ ] bootstrap install order is deterministic and benchmarked
- [ ] mdBook contributor docs updated
- [ ] gates green

## Goal

Give engine and extension authors a high-level, production-ready API for
adding JavaScript-visible objects, namespaces, classes, functions,
accessors, and hosted module surfaces without paying runtime overhead on
the hot path or slowing cold startup unnecessarily.

Breaking Rust API changes inside `crates-next/*` are allowed in this task.
The priority is production readiness: simple invariants, fast startup,
fast steady-state dispatch, and contributor APIs that are hard to misuse.

## Source

- [`70-gc-master-tracker.md`](./70-gc-master-tracker.md) explicit-context,
  GC-rooting, and production-gate rules.
- [`91-gc-bench-and-soak-infra.md`](./91-gc-bench-and-soak-infra.md)
  benchmark and soak infrastructure.
- [`95-contributor-book-and-extension-guides.md`](./95-contributor-book-and-extension-guides.md)
  mdBook contributor documentation rule.
- Boa's builder-style API shape is a reference point, not a dependency:
  high-level builders are useful, but Otter's builders must preserve
  explicit `RuntimeCx` / `NativeCtx`, write barriers, and single-mutator
  ownership.

## Scope

### 96.1 — Static JS surface specs

Add static spec records for JavaScript-visible API shape:

```rust
pub struct Attr {
    pub writable: bool,
    pub enumerable: bool,
    pub configurable: bool,
}

pub struct MethodSpec {
    pub name: &'static str,
    pub length: u8,
    pub attrs: Attr,
    pub call: NativeCall,
}

pub struct NamespaceSpec {
    pub name: &'static str,
    pub methods: &'static [MethodSpec],
    pub accessors: &'static [AccessorSpec],
    pub constants: &'static [ConstSpec],
}
```

Names may change, but the constraints do not:

- exported JS names and arity are explicit in static data;
- attributes are explicit, never hidden in ad-hoc helper defaults;
- specs are cheap to inspect and can be reused by docs/type generation;
- specs contain no `Gc<T>`, `Local<'gc, T>`, `Frame`, or isolate-local
  handles.

### 96.2 — Mutator-bound builders

Add builder APIs that install specs during a mutator turn:

```rust
NamespaceBuilder::new(cx, "Math")?
    .method("abs", 1, math_abs, Attr::builtin_function())?
    .constant("PI", Value::Number(pi), Attr::read_only())?
    .build()?;
```

Builder values must be lifetime-bound to `RuntimeCx<'_>` or
`NativeCtx<'_>` and must be `!Send + !Sync`. They may allocate and mutate
only through explicit context APIs so write barriers and heap ownership are
visible in the type system.

Required builders:

- `ObjectBuilder` for plain object / module namespace construction;
- `FunctionBuilder` for native function objects with `name`, `length`,
  prototype / constructor metadata, and attributes;
- `ConstructorBuilder` for constructor + prototype + backlink wiring;
- `ClassBuilder` for constructor-backed Web/ES classes;
- `NamespaceBuilder` for global namespaces and hosted module namespace
  objects.

### 96.3 — Native builtin fast path

Split native callable storage so production builtins do not pay closure
boxing or dynamic dispatch when a plain function pointer is enough:

```rust
pub type NativeFastFn =
    for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>;

pub enum NativeCall {
    Static(NativeFastFn),
    Dynamic(Box<NativeFn>),
}
```

Exact names may change. The invariant is that spec-declared builtins and
macro-generated builtins use the static function-pointer path by default.
Dynamic boxed closures remain available for rare host/embedder cases that
need captured Rust state.

### 96.4 — Central bootstrap registry

Centralize installation of globals, constructors, prototypes, namespaces,
and hosted module surfaces through a bootstrap registry. Do not scatter
ad-hoc global mutation across feature modules.

The registry should support:

- deterministic install order;
- feature flags / capability gating at install time;
- lazy or tiered installation where this improves cold startup;
- build-time or startup-time validation of duplicate names, duplicate
  prototype methods, invalid arity, and invalid attributes.

### 96.5 — Migration slice

Migrate a small but representative set first:

1. one namespace with constants and static functions (`Math` or `Reflect`);
2. one constructor/prototype surface (`TextEncoder`, `RegExp`, or a small
   ES class-shaped builtin);
3. one hosted-module namespace once hosted modules are active in
   `crates-next`.

Do not mass-migrate all builtins until the benchmarks show no regression.

### 96.6 — mdBook documentation

Update the mdBook as part of this task:

- `docs/book/src/engine/architecture.md` explains centralized bootstrap;
- `docs/book/src/extensions/overview.md` explains builders and specs;
- add or update an extension/native-binding page with buildable examples;
- task files remain implementation history, not the main contributor API
  docs.

## Performance requirements

This task is only successful if the high-level API compiles down to the
same runtime shape we would write by hand.

Required gates:

- native builtin call overhead benchmark: static path is not slower than
  the previous handwritten path beyond noise;
- cold `RuntimeBuilder::build()` benchmark before/after;
- cold first `run_script("undefined;")` benchmark before/after;
- bootstrap allocation count before/after;
- no per-call allocation for static builtins;
- no `HashMap<String, Box<dyn Fn...>>` or equivalent runtime registry in
  the JS hot path;
- no string parsing of metadata at runtime.

## Out of scope

- Proc macros. Task 97 adds macros after this backend has proven the
  generated shape.
- Worker/plugin ABI stability.
- Mass migration of every builtin.
- JIT integration.

## Validation gates

- [ ] `cargo test -p otter-vm -p otter-runtime` green.
- [ ] `cargo clippy --workspace --all-targets --all-features -- -D warnings` clean.
- [ ] Benchmarks show no statistically meaningful steady-state regression
  for static native builtin calls.
- [ ] Startup benchmarks have an explicit before/after table in the PR or
  task closeout notes.
- [ ] `rg "GcHeap::with_thread_default|enter_thread_default" crates-next/otter-vm crates-next/otter-runtime` has no product-code hits.
- [ ] mdBook builds and documents the new contributor-facing API.

## Closing

Update this task, the task index, and mdBook pages. If public names differ
from the examples above, document the final names in the book and keep the
performance invariants unchanged.
