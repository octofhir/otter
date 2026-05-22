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

use smallvec::SmallVec;

use crate::js_surface::{Attr, MethodSpec};
use crate::native_function::NativeCall;
use crate::number::NumberValue;
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

fn native_is_array(_: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::Boolean(matches!(
        args.first(),
        Some(Value::Array(_))
    )))
}

/// §23.1.2.3 `Array.of(...items)` JS-visible NativeFunction.
fn native_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let this_value = *ctx.this_value();
    if is_constructor(&this_value) {
        return construct_and_fill(ctx, &this_value, args)
            .map_err(|e| vm_to_native_array_static("Array.of", e));
    }
    let arr = ctx
        .array_from_elements_with_roots(args.iter().cloned(), &[], &[args])
        .map_err(|_| NativeError::TypeError {
            name: "Array.of",
            reason: "out of memory while allocating array".to_string(),
        })?;
    Ok(Value::Array(arr))
}

/// §23.1.2.1 `Array.from(items, mapFn?, thisArg?)` JS-visible
/// NativeFunction. Routes through
/// `Interpreter::array_from_sync` so the iterable / array-like
/// ladder observes user `@@iterator` / `mapFn` / `thisArg`.
fn native_from(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| NativeError::TypeError {
        name: "Array.from",
        reason: "missing execution context".to_string(),
    })?;
    interp
        .array_from_sync(&exec, args)
        .map_err(|e| vm_to_native_array_static("Array.from", e))
}

fn is_constructor(value: &Value) -> bool {
    matches!(
        value,
        Value::Function { .. }
            | Value::Closure(_)
            | Value::NativeFunction(_)
            | Value::BoundFunction(_)
            | Value::ClassConstructor(_)
    )
}

/// §23.1.2.3 step 4–7: `Construct(C, «len»)` then write each item
/// via `CreateDataPropertyOrThrow`.
fn construct_and_fill(
    ctx: &mut NativeCtx<'_>,
    target: &Value,
    args: &[Value],
) -> Result<Value, VmError> {
    let len = args.len();
    let mut ctor_args: SmallVec<[Value; 8]> = SmallVec::with_capacity(1);
    ctor_args.push(Value::Number(NumberValue::from_i32(len as i32)));
    let receiver = {
        let (interp, exec) = ctx.interp_mut_and_context();
        let exec = exec.ok_or(VmError::InvalidOperand)?;
        interp.run_construct_sync(&exec, target, *target, ctor_args)?
    };
    let receiver_obj = match &receiver {
        Value::Object(obj) => *obj,
        Value::Array(_) => return Ok(receiver),
        _ => {
            return Err(VmError::TypeError {
                message: "Array.of constructor returned a non-object".to_string(),
            });
        }
    };
    for (idx, value) in args.iter().enumerate() {
        let key = idx.to_string();
        ctx.set_property(receiver_obj, &key, *value)?;
    }
    ctx.set_property(
        receiver_obj,
        "length",
        Value::Number(NumberValue::from_i32(len as i32)),
    )?;
    Ok(receiver)
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
