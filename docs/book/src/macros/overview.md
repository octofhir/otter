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

- `#[dive]` or equivalent async native binding sugar;
- host-owned object surface macros;
- hosted-module loader macros;
- GC trace derive macros.

Macros must not hide capability enforcement, bootstrap order, async host-op
scheduling, or important control flow. Prefer manual code when those
concerns are the main behavior.

## Generated Shape Contract

Macro expansion must produce:

- static spec records from Task 96;
- ordinary Rust functions with explicit exported JS names and arity;
- static native call targets for builtins by default;
- builder/bootstrap calls through the centralized registry.

Macro expansion must not produce:

- runtime registries or metadata parsing;
- per-call allocation;
- hidden global mutation;
- hidden permission checks;
- hidden async scheduling;
- captures of `RuntimeCx`, `NativeCtx`, `Value`, `Frame`, `Gc<T>`, or
  `Local<'gc, T>` across `.await`.

## Example Source And Expansion

Potential future source:

```rust,ignore
#[js_namespace(name = "Math")]
mod math {
    #[js_fn(name = "abs", length = 1)]
    fn abs(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        math_abs(ctx, args)
    }
}
```

Required generated shape:

```rust,ignore
fn math_abs_export(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    math_abs(ctx, args)
}

static MATH_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Math",
    methods: &[MethodSpec {
        name: "abs",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(math_abs_export),
    }],
    accessors: &[],
    constants: &[],
};
```

The exact names may change in Task 96/97. The invariant is that reviewers
can inspect expansion and see the same runtime shape as handwritten specs.

## Prefer Manual Code When

- capability enforcement is the main behavior;
- bootstrap or install order needs careful review;
- async scheduling or cancellation behavior is non-trivial;
- the macro would hide object graph, rooting, or external-memory lifetime
  decisions.
