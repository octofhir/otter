---
title: "JS Surface Builders"
---

Otter's preferred contributor API for JavaScript-visible surfaces is a
static spec plus mutator-bound builder flow.

The examples below document the required generated/runtime shape. Product
crates should use the runtime-owned facade in `otter_runtime`; the VM keeps the
static backend and centralized bootstrap internals. `Math`, `JSON`, `Atomics`,
`console`, and active Web API classes are installed through this path.

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

```rust
use otter_runtime::{
    RuntimeAttr as Attr, RuntimeConstValue as ConstValue,
    RuntimeConstSpec, RuntimeMethodSpec, RuntimeNamespaceSpec as NamespaceSpec,
    RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeValue as Value, runtime_constant, runtime_method, runtime_namespace,
};

fn math_abs(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let _ = (ctx, args);
    Ok(Value::Undefined)
}

static MATH_METHODS: [RuntimeMethodSpec; 1] = [runtime_method("abs", 1, math_abs)];

static MATH_CONSTANTS: [RuntimeConstSpec; 1] =
    [runtime_constant("PI", ConstValue::Number(std::f64::consts::PI))];

static MATH_SPEC: NamespaceSpec = runtime_namespace(
    "Math",
    &MATH_METHODS,
    &[],
    &MATH_CONSTANTS,
    Attr::global_binding(),
);
```

Builders are lifetime-bound to the current mutator turn:

```rust,ignore
let mut object = otter_runtime::RuntimeObjectBuilder::new(ctx)?;
object.builtin_method("abs", 1, math_abs)?;
let namespace = object.build();
```

Do not store builders, contexts, `Value`, `Gc<T>`, `Local<'gc, T>`, or VM
frames in async host futures.

## Spec Records

- `RuntimeAttr`: writable/enumerable/configurable attributes with explicit
  defaults and helpers such as `builtin_function`, `read_only`, and
  `global_binding`;
- `RuntimeConstValue` / `RuntimeConstSpec`: static primitive values and constant/data
  properties;
- `RuntimePropertySpec`: data properties and constants;
- `RuntimeMethodSpec`: exported name, `.length`, attributes, and native call
  target;
- `RuntimeAccessorSpec`: getter/setter pair and accessor attributes;
- `RuntimeConstructorSpec`: constructor function, prototype, statics, and
  prototype methods;
- `RuntimeClassSpec`: class-shaped constructor/prototype/static surface;
- `RuntimeNamespaceSpec`: namespace object or hosted module namespace.

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

Native call storage is split into a static fast path and a dynamic path:

```rust,ignore
use otter_runtime::{RuntimeNativeCall, RuntimeNativeFastFn, RuntimeNativeFn};
use std::sync::Arc;

pub enum RuntimeNativeCall {
    Static(RuntimeNativeFastFn),
    Dynamic(Arc<RuntimeNativeFn>),
}
```

Spec-declared builtins and macro-generated builtins should use
`runtime_method(...)`, `RuntimeObjectBuilder::builtin_method(...)`, or
`runtime_native_static(...)` by default. Use dynamic closures only when the
embedder needs captured Rust state, and keep traced JS captures explicit.
Crate-internal VM helpers may still use local unchecked constructors for
audited isolate-local payloads.

## Current Migration

The first migrated namespaces are `Math`, `JSON`, `Atomics`, and
`console`:

- `globalThis.Math` is installed from `math::MATH_SPEC`;
- Math constants use non-writable, non-enumerable, non-configurable
  descriptors;
- Math methods are `Value::NativeFunction` values using the static
  function-pointer path and explicit `.length`;
- direct `Math.abs(...)` calls still use the existing `Op::MathCall`
  compiler fast path, while method reads such as `Math.abs.length` and
  extracted calls use the installed namespace object.
- `globalThis.JSON`, `globalThis.Atomics`, and `globalThis.console` are
  installed by the centralized bootstrap registry from static namespace
  specs;
- `console` output is routed through an embedder-overridable
  `ConsoleSink`; the default sink writes with `println!` / `eprintln!`.
- Active Web API classes use `runtime_class`, `runtime_constructor`,
  `runtime_method`, and `RuntimeObjectBuilder::from_host_data` from
  `otter_runtime`. Embedders can enable the active Web globals with
  `otter_web::WebApiBuilderExt::with_web_apis`; the CLI enables that preset by
  default while keeping `otter-runtime` independent of `otter-web`.

## Performance Rules

High-level API work must preserve the handwritten runtime shape:

- no per-call allocation for static builtins;
- no runtime parsing of metadata;
- no hot-path `HashMap<String, Box<dyn Fn...>>` registry;
- no hidden global mutation outside centralized bootstrap;
- no hidden permission or async scheduling logic in builders.

Changes that affect builtin installation or native call dispatch need
before/after benchmark notes for startup and steady-state native calls.
