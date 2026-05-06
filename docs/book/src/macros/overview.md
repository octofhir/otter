# Macro Overview

Macros are contributor ergonomics over the static JS surface backend, not a
separate runtime registration system.

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

The available Task 97 slice is in `otter-macros`:

- `#[js_namespace]` generates namespace specs from inline Rust modules;
- `#[js_class]` generates constructor/prototype class specs;
- `raft!` generates grouped namespace specs without helper attributes.

`#[js_namespace]` example:

```rust,ignore
use otter_macros::js_namespace;

#[js_namespace(name = "Math", spec = MATH_SPEC)]
mod math {
    #[js_fn(name = "abs", length = 1)]
    pub fn abs(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        math_abs(ctx, args)
    }
}
```

The macro emits a public `MATH_SPEC: NamespaceSpec` and private static
method metadata that can be passed to the Task 96 builders or installed by
the centralized bootstrap registry.

`#[js_class]` separates ordinary instance methods from JavaScript static
methods:

```rust,ignore
use otter_macros::js_class;

#[js_class(name = "Point", spec = POINT_SPEC)]
mod point {
    #[js_constructor(length = 1)]
    pub fn construct(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        create_point(ctx, args)
    }

    #[js_method(name = "valueOf", length = 0)]
    pub fn value_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        point_value_of(ctx, args)
    }

    #[js_static_method(name = "from", length = 1)]
    pub fn from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        point_from(ctx, args)
    }

    #[js_getter(name = "x")]
    pub fn get_x(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        point_get_x(ctx, args)
    }
}
```

Here `#[js_method]` installs `Point.prototype.valueOf`, while
`#[js_static_method]` installs `Point.from`. Both still use the
`NativeCall::Static` Rust function-pointer path.

`raft!` is for grouped namespace declarations:

```rust,ignore
otter_macros::raft! {
    pub static MATH_SPEC: namespace("Math") {
        methods: [
            "abs" => math_abs, length = 1;
        ]
    }
}
```

Remaining planned macro scope:

- macro coverage for constants/data properties where the generated shape is
  still inspectable;
- broader migration only after benchmark ratchets are in place.

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

Source:

```rust,ignore
#[js_namespace(name = "Math", spec = MATH_SPEC)]
mod math {
    #[js_fn(name = "abs", length = 1)]
    pub fn abs(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
        math_abs(ctx, args)
    }
}
```

Required generated shape:

```rust,ignore
static MATH_SPEC: NamespaceSpec = NamespaceSpec {
    name: "Math",
    methods: &[MethodSpec {
        name: "abs",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(math::abs),
    }],
    accessors: &[],
    constants: &[],
    attrs: Attr::global_binding(),
};
```

Keep manual Task 96 specs for capability gates, delicate bootstrap order,
async scheduling, host-owned object lifetimes, or API shapes not covered by
the current macros.

## Prefer Manual Code When

- capability enforcement is the main behavior;
- bootstrap or install order needs careful review;
- async scheduling or cancellation behavior is non-trivial;
- the macro would hide object graph, rooting, or external-memory lifetime
  decisions.
