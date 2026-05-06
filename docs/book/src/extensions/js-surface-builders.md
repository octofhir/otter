# JS Surface Builders

Otter's preferred contributor API for JavaScript-visible surfaces is a
static spec plus mutator-bound builder flow.

This API is planned for Task 96. The examples below document the required
generated/runtime shape; they are intentionally marked `ignore` until the
Task 96 names land.

The goal is high-level ergonomics without runtime overhead:

- exported JavaScript names, arity, and attributes live in static specs;
- builders install those specs through `RuntimeCx` / `NativeCtx` during a
  mutator turn;
- all property writes go through the object model so write barriers fire;
- production builtins use a static native function-pointer path by
  default;
- dynamic boxed closures are reserved for rare host/embedder cases that
  need captured Rust state;
- bootstrap install order is centralized and deterministic.

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

## Spec Records

Task 96 owns the final names, but the records must cover:

- `Attr`: writable/enumerable/configurable attributes with explicit
  defaults;
- `PropertySpec`: data properties and constants;
- `MethodSpec`: exported name, `.length`, attributes, and native call
  target;
- `AccessorSpec`: getter/setter pair and accessor attributes;
- `ConstructorSpec`: constructor function, prototype, statics, and
  prototype methods;
- `ClassSpec`: class-shaped constructor/prototype/static surface;
- `NamespaceSpec`: namespace object or hosted module namespace.

Specs contain only static metadata and native targets. They must not hold
`Gc<T>`, `Local<'gc, T>`, `RuntimeCx`, `NativeCtx`, VM frames, or runtime
locks.

## Builders And Bootstrap

Builders install specs during a mutator turn through explicit context
APIs. They may allocate and mutate JS objects, but they must perform those
stores through the object model so barriers fire.

The centralized bootstrap registry owns deterministic install order,
duplicate-name validation, feature/capability gating, and any lazy/tiered
installation choices. Do not scatter ad-hoc global mutation across builtin
modules.

## Native Calls

Task 96 should split native call storage into a static fast path and a
dynamic path:

```rust,ignore
pub enum NativeCall {
    Static(NativeFastFn),
    Dynamic(Box<NativeFn>),
}
```

Spec-declared builtins and macro-generated builtins should use
`NativeCall::Static` by default. Use dynamic boxed closures only when the
embedder needs captured Rust state, and keep traced JS captures explicit.

## Performance Rules

High-level API work must preserve the handwritten runtime shape:

- no per-call allocation for static builtins;
- no runtime parsing of metadata;
- no hot-path `HashMap<String, Box<dyn Fn...>>` registry;
- no hidden global mutation outside centralized bootstrap;
- no hidden permission or async scheduling logic in builders.

Changes that affect builtin installation or native call dispatch need
before/after benchmark notes for startup and steady-state native calls.
