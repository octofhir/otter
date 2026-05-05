# JS Surface Builders

Otter's preferred contributor API for JavaScript-visible surfaces is a
static spec plus mutator-bound builder flow.

The goal is high-level ergonomics without runtime overhead:

- exported JavaScript names, arity, and attributes live in static specs;
- builders install those specs through `RuntimeCx` / `NativeCtx` during a
  mutator turn;
- all property writes go through the object model so write barriers fire;
- production builtins use a static native function-pointer path by default;
- dynamic boxed closures are reserved for rare host/embedder cases that
  need captured Rust state;
- bootstrap install order is centralized and deterministic.

This page is the contributor-facing home for task 96 once the API lands.
Task files describe implementation history; examples and stable workflow
belong here.

## Shape

The intended model is:

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
    constants: &[
        ConstSpec {
            name: "PI",
            attrs: Attr::read_only(),
            value: ConstValue::Number(std::f64::consts::PI),
        },
    ],
};
```

Builders are lifetime-bound to the current mutator turn:

```rust,ignore
NamespaceBuilder::from_spec(cx, &MATH_SPEC)?.build()?;
```

Do not store builders, contexts, `Value`, `Gc<T>`, `Local<'gc, T>`, or VM
frames in async host futures.

## Performance Rules

High-level API work must preserve the handwritten runtime shape:

- no per-call allocation for static builtins;
- no runtime parsing of metadata;
- no hot-path `HashMap<String, Box<dyn Fn...>>` registry;
- no hidden global mutation outside centralized bootstrap;
- no hidden permission or async scheduling logic in builders.

Changes that affect builtin installation or native call dispatch need
before/after benchmark notes for startup and steady-state native calls.
