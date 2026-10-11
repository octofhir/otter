//! `Array.<static>` dispatchers and JS-visible static method specs.
//!
//! Each static method resolves through its actual callable and receives the
//! original receiver. Rooted constructor, iterator, property, and callback
//! semantics live in `crate::array_ops`.
//!
//! # Contents
//! - [`ARRAY_STATIC_METHODS`] — methods installed on the `Array`
//!   constructor during bootstrap.
//! - `Array.fromAsync` dispatches into [`crate::array_from_async`], which
//!   owns the promise chain the collection runs on.
//!
//! # Invariants
//! - The NativeFunction path observes the spec `this` value, so
//!   `Array.of.call(C, ...)` runs `Construct(C, «len»)` per
//!   §23.1.2.3 step 4 when `C` is a constructor.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
//! - <https://tc39.es/ecma262/#sec-array>
//! - <https://tc39.es/ecma262/#sec-array.from>
//! - <https://tc39.es/ecma262/#sec-array.fromasync>
//! - <https://tc39.es/ecma262/#sec-array.of>

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::{NativeCtx, NativeError, Value};

/// Static methods installed on the `Array` constructor.
pub static ARRAY_STATIC_METHODS: &[MethodSpec] = &[
    MethodSpec {
        name: "isArray",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(native_is_array),
    },
    MethodSpec {
        name: "of",
        length: 0,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(native_of),
    },
    MethodSpec {
        name: "from",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(native_from),
    },
    MethodSpec {
        name: "fromAsync",
        length: 1,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(native_from_async),
    },
];

fn native_is_array(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    // §7.2.2 IsArray includes Proxy recursion and revoked-Proxy TypeError.
    let result = ctx.cx.interp.is_array(&value).map_err(|err| {
        crate::native_function::vm_to_native_error(ctx.cx.interp, err, "Array.isArray")
    })?;
    Ok(Value::boolean(result))
}

/// §23.1.2.2 `Array.of(...items)` JS-visible NativeFunction. Routes
/// through `Interpreter::array_of_sync` so `Array.of.call(C, …)`
/// observes the `this` constructor `C` (`Construct(C, «len»)` plus the
/// spec `CreateDataPropertyOrThrow` / `Set(A, "length", …)` protocol).
fn native_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Array.of",
            reason: "missing execution context".to_string(),
        })?;
    ctx.with_turn_parts(|interp, stack| {
        let result = interp.array_of_sync(stack, &exec, this_value, args);
        result.map_err(|e| e.into_native(interp, "Array.of"))
    })
}

/// §23.1.2.1 `Array.from(items, mapFn?, thisArg?)` JS-visible
/// NativeFunction. Routes through
/// `Interpreter::array_from_sync` so the iterable / array-like
/// ladder observes user `@@iterator` / `mapFn` / `thisArg`.
fn native_from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let exec = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Array.from",
            reason: "missing execution context".to_string(),
        })?;
    ctx.with_turn_parts(|interp, stack| {
        let result = interp.array_from_sync(stack, &exec, this_value, args);
        result.map_err(|error| error.into_native(interp, "Array.from"))
    })
}

/// §23.1.2.1 `Array.fromAsync(items, mapFn?, thisArg?)` — collect an async
/// iterable, a sync iterable, or an array-like of promises into a new array,
/// answering with the promise for it.
fn native_from_async(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    crate::array_from_async::native_from_async(ctx, args)
}
