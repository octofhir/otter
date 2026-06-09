//! `Array.<static>` dispatchers and JS-visible static method specs.
//!
//! Each Array static surface has its own typed entry point —
//! The active `Array(...)`, `Array.from`, and `Array.of` opcode
//! paths live in [`crate::array_ops`] because they can expose the VM
//! frame stack to root-aware allocation. This module owns the
//! JS-visible static method specs installed on the constructor so
//! reflective access (`Array.of.length`, `Array.of.call(C, ...)`,
//! `Array.from.bind(...)`) resolves to a real callable.
//!
//! # Contents
//! - [`ARRAY_STATIC_METHODS`] — methods installed on the `Array`
//!   constructor during bootstrap.
//!
//! # Invariants
//! - The compiler's [`otter_bytecode::Op::ArrayOf`] /
//!   [`otter_bytecode::Op::ArrayFrom`] fast paths still bypass the
//!   NativeFunction dispatcher for direct `Array.<x>(...)` callsites.
//! - The NativeFunction path observes the spec `this` value, so
//!   `Array.of.call(C, ...)` runs `Construct(C, «len»)` per
//!   §23.1.2.3 step 4 when `C` is a constructor.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-array-constructor>
//! - <https://tc39.es/ecma262/#sec-array>
//! - <https://tc39.es/ecma262/#sec-array.from>
//! - <https://tc39.es/ecma262/#sec-array.of>

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::{NativeCtx, NativeError, Value, VmError};

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
];

fn native_is_array(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = args.first().copied().unwrap_or_else(Value::undefined);
    // §7.2.2 IsArray — shared with the `Op::IsArray` fast path so the
    // direct call and `Array.isArray.call(...)` agree (Proxy recursion,
    // revoked-Proxy TypeError).
    let result = crate::abstract_ops::is_array(ctx.heap(), &value)
        .map_err(|err| crate::native_function::vm_to_native_error(err, "Array.isArray"))?;
    Ok(Value::boolean(result))
}

/// §23.1.2.2 `Array.of(...items)` JS-visible NativeFunction. Routes
/// through `Interpreter::array_of_sync` so `Array.of.call(C, …)`
/// observes the `this` constructor `C` (`Construct(C, «len»)` plus the
/// spec `CreateDataPropertyOrThrow` / `Set(A, "length", …)` protocol).
fn native_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: "Array.of",
        reason: "missing execution context".to_string(),
    })?;
    interp
        .array_of_sync(&exec, this_value, args)
        .map_err(|e| vm_to_native_array_static("Array.of", e))
}

/// §23.1.2.1 `Array.from(items, mapFn?, thisArg?)` JS-visible
/// NativeFunction. Routes through
/// `Interpreter::array_from_sync` so the iterable / array-like
/// ladder observes user `@@iterator` / `mapFn` / `thisArg`.
fn native_from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: "Array.from",
        reason: "missing execution context".to_string(),
    })?;
    interp
        .array_from_sync(&exec, this_value, args)
        .map_err(|e| vm_to_native_array_static("Array.from", e))
}

fn vm_to_native_array_static(name: &'static str, err: VmError) -> NativeError {
    match err {
        VmError::Uncaught { value } => NativeError::Thrown {
            name,
            message: value,
        },
        VmError::TypeError { message } => NativeError::TypeError {
            name,
            reason: message,
        },
        VmError::RangeError { message } => NativeError::RangeError {
            name,
            reason: message,
        },
        other => NativeError::TypeError {
            name,
            reason: other.to_string(),
        },
    }
}
