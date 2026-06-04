---
title: "Adding a Built-in Prototype Method"
---

This page is the canonical recipe for adding (or changing) a method on a
built-in prototype — `Number.prototype.toFixed`, `Array.prototype.map`,
`Map.prototype.get`, and so on. Follow it and your method lands on the
same dispatch path as every other built-in.

There is **one** shape for a built-in method: a native function with the
[Native Call ABI](/otter/engine/native-call-abi/) signature, installed on
the prototype through the `couch!` surface.

```rust
fn proto_method(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // 1. brand-check / unwrap the receiver
    // 2. coerce arguments (re-entrant ToNumber / ToString as the spec asks)
    // 3. do the work, allocate via ctx.heap_mut()
    // 4. return Ok(value) or Err(NativeError::…)
}
```

`NativeCtx` is a re-entrant handle: it exposes the receiver
(`ctx.this_value()`), the GC heap (`ctx.heap()` / `ctx.heap_mut()`), and
the interpreter + execution context (`ctx.interp_mut()`,
`ctx.execution_context()`) so a method can run user-observable
algorithms — a `valueOf` on an argument, a getter on a generic
array-like receiver, a callback.

## 1. Write the native

Put it in the type's `prototype.rs` (`number/prototype.rs`,
`boolean/prototype.rs`, …). Brand-check the receiver with a shared
`this<Type>Value` helper that accepts both the primitive and its wrapper
object:

```rust
/// §21.1.3 thisNumberValue(value)
fn this_number_value(ctx: &NativeCtx<'_>, name: &'static str) -> Result<NumberValue, NativeError> {
    let this = *ctx.this_value();
    if let Some(n) = this.as_number() {
        return Ok(n);
    }
    if let Some(obj) = this.as_object()
        && let Some(n) = crate::object::number_data(obj, ctx.heap())
    {
        return Ok(n);
    }
    Err(NativeError::TypeError {
        name,
        reason: "Number.prototype method called on incompatible receiver".to_string(),
    })
}
```

Coerce arguments **inside** the method, in spec order. When the
coercion can run user code (`ToNumber` / `ToString` on an object
argument observes `@@toPrimitive` / `valueOf` / `toString`), re-enter
through the context:

```rust
let exec = ctx.execution_context().cloned();
// …
let interp = ctx.interp_mut();
let primitive = interp.evaluate_to_primitive(&exec, arg, ToPrimitiveHint::Number)?;
```

Allocate strings/objects through the heap; `?` converts
`otter_gc::OutOfMemory` to `NativeError` automatically:

```rust
Ok(Value::string(JsString::from_str(&rendered, ctx.heap_mut())?))
```

Errors are `NativeError` variants — pick the spec class
(`TypeError` / `RangeError`); `Thrown { message }` re-raises a JS value
a re-entrant call already produced.

## 2. Register it on the prototype

Each method becomes a `MethodSpec`; collect them in a `static` slice and
hand the slice to the type's `couch!` block:

```rust
pub static NUMBER_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 1, number_to_string),
    method("toFixed", 1, number_to_fixed),
    method("valueOf", 0, number_value_of),
];

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec { name, length, attrs: Attr::builtin_function(), call: NativeCall::Static(call) }
}
```

```rust
otter_macros::couch! {
    name = "Number",
    feature = CORE,
    constructor = (length = 1, call = number_ctor_call),
    prototype = { method_specs = [super::prototype::NUMBER_PROTOTYPE_METHODS] },
}
```

Small surfaces can inline the rows instead:
`prototype = { methods = { "exec" / 1 => proto_exec, "test" / 1 => proto_test } }`.
See the [`couch!` design notes](/otter/macros/design/) for the full field
list (accessors, `parent`, constants, statics).

## 3. The dispatch gate

`Op::CallMethodValue` resolves `obj.method(...)` through §7.3.11
`GetMethod` + §7.3.14 `Call` against the installed prototype native —
nothing else to wire. For primitive receivers (number, boolean, …) the
`has_plain_builtin_method` gate in `method_ops.rs` front-runs that
resolution; point its arm for your type at the same `MethodSpec` slice
so there is a single source of truth for "which names are built-ins":

```rust
if recv_value.is_number() {
    return number::prototype::NUMBER_PROTOTYPE_METHODS
        .iter()
        .any(|m| m.name == name);
}
```

## 4. Gate the change

Built-in work is conformance-gated. Capture a `test262` baseline for the
affected directory before and after (stash → run → pop → run → diff) and
land only equal-or-better with no new crash / timeout / OOM:

```bash
cargo build -p otter-vm
cargo test -p otter-vm
cargo clippy -p otter-vm --all-targets --all-features -- -D warnings
./target/debug/otter-test262 run --filter "built-ins/Number" --output after.json
```

