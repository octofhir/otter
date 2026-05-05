# Macro Overview

Macros are planned as contributor ergonomics, not as a separate runtime
registration system.

The required order is:

1. task 96 lands static specs, mutator-bound builders, and centralized
   bootstrap;
2. task 97 adds macros that generate those static specs and normal Rust
   functions;
3. task 98 protects startup and first-run performance with benchmark
   ratchets.

Macros must be zero-cost at runtime. Generated code should look like
handwritten static specs:

```rust,ignore
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
};
```

Initial macro scope after the backend is stable:

- `#[js_namespace]` for namespace objects;
- `#[js_class]` for constructor/prototype/static method surfaces;
- `raft!` or equivalent grouped-spec declaration.

Deferred until their backend APIs are stable:

- async native binding sugar;
- host-owned object surface macros;
- hosted-module loader macros;
- GC trace derive macros.

Macros must not hide capability enforcement, bootstrap order, async host-op
scheduling, or important control flow. Prefer manual code when those
concerns are the main behavior.
