//! `%Date%` constructor installer.
//!
//! Implements ECMA-262 §21.4 Date Objects: the `Date()` constructor and
//! its prototype wiring.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-date-objects>

use smallvec::SmallVec;

use crate::abstract_ops::{self, ToPrimitiveHint};
use crate::date::now_ms;
use crate::{JsString, NativeCtx, NativeError};
use crate::{Value, VmError};

// §21.4.3 Date statics — trampolines that route to the typed
// dispatcher with no `this`. The constructor's `[[Construct]]` /
// `[[Call]]` slot still handles the `Date(...)` and `new Date(...)`
// shapes via `date_ctor_call`.
fn date_now_call(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    crate::date::dispatch::call_static(otter_bytecode::method_id::DateMethod::Now, &[], ctx.heap())
        .map_err(|err| NativeError::TypeError {
            name: "Date.now",
            reason: err.to_string(),
        })
}
fn date_parse_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    crate::date::dispatch::call_static(
        otter_bytecode::method_id::DateMethod::Parse,
        args,
        ctx.heap(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "Date.parse",
        reason: err.to_string(),
    })
}
fn date_utc_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced;
    let date_args = if args.is_empty() {
        args
    } else {
        coerced = coerce_number_args(ctx, "Date.UTC", args)?;
        &coerced
    };
    crate::date::dispatch::call_static(
        otter_bytecode::method_id::DateMethod::UTC,
        date_args,
        ctx.heap(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "Date.UTC",
        reason: err.to_string(),
    })
}

fn date_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    if !ctx.is_construct_call() {
        // §21.4.2.1 — `Date(...)` as a function ignores its arguments
        // and returns the current time rendered as a string.
        let text = crate::date::prototype::local_date_time_string(now_ms())
            .unwrap_or_else(|| "Invalid Date".to_string());
        let value =
            JsString::from_str(&text, ctx.heap_mut()).map_err(|err| NativeError::TypeError {
                name: "Date",
                reason: err.to_string(),
            })?;
        return Ok(Value::string(value));
    }

    let time = date_construct_time_value(ctx, args)?;
    // §21.4.2.1 — `new Date(...)`. The construct receiver is a
    // freshly allocated JsObject (via OrdinaryCreateFromConstructor
    // on `Date`). Install the `[[DateValue]]` internal slot and return it.
    if let Some(obj) = ctx.this_value().as_object() {
        crate::object::set_date_data(obj, ctx.heap_mut(), time);
        return Ok(Value::object(obj));
    }
    Err(NativeError::TypeError {
        name: "Date",
        reason: "expected object receiver in `new Date(...)`".to_string(),
    })
}

fn date_construct_time_value(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<f64, NativeError> {
    if args.is_empty() {
        return Ok(crate::date::dispatch::construct_time_value(
            args,
            ctx.heap(),
        ));
    }

    if args.len() > 1 {
        let coerced = coerce_number_args(ctx, "Date", args)?;
        return Ok(crate::date::dispatch::construct_time_value(
            &coerced,
            ctx.heap(),
        ));
    }

    let value = args[0];
    if let Some(obj) = value.as_object()
        && let Some(time) = crate::object::date_data(obj, ctx.heap())
    {
        return Ok(time);
    }

    let primitive = if abstract_ops::is_primitive(&value) {
        value
    } else {
        let context = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "Date",
                reason: "missing execution context".to_string(),
            })?;
        ctx.cx
            .interp
            .coerce_to_primitive(&context, &value, ToPrimitiveHint::Default)
            .map_err(date_vm_error)?
    };

    if primitive.as_string(ctx.heap()).is_some() {
        return Ok(crate::date::dispatch::construct_time_value(
            &[primitive],
            ctx.heap(),
        ));
    }

    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name: "Date",
            reason: "missing execution context".to_string(),
        })?;
    ctx.cx
        .interp
        .coerce_to_number(&context, &primitive)
        .map(|n| n.as_f64())
        .map_err(date_vm_error)
}

fn coerce_number_args(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    args: &[Value],
) -> Result<SmallVec<[Value; 8]>, NativeError> {
    let context = ctx
        .execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })?;
    let mut coerced = SmallVec::with_capacity(args.len());
    for value in args {
        let number = ctx
            .cx
            .interp
            .coerce_to_number(&context, value)
            .map_err(date_vm_error)?;
        coerced.push(Value::number(number));
    }
    Ok(coerced)
}

fn date_vm_error(err: VmError) -> NativeError {
    match err {
        VmError::TypeError { message } => NativeError::TypeError {
            name: "Date",
            reason: message,
        },
        VmError::Uncaught { value } => NativeError::Thrown {
            name: "Date",
            message: value,
        },
        other => NativeError::TypeError {
            name: "Date",
            reason: other.to_string(),
        },
    }
}

// `DATE_SPEC` + `Intrinsic` generated by `couch!`. §21.4 — Date
// constructor (`new Date(...)` produces a DateValue-bearing
// JsObject, bare `Date()` returns the same string shape as
// `(new Date()).toString()`). Statics
// (`now` / `parse` / `UTC`) trampoline to the typed dispatcher
// above. Prototype methods come from the pre-built
// `DATE_PROTOTYPE_METHODS` + `DATE_PROTOTYPE_EXTRA_METHODS`
// slices generated by the `date_prototype_methods!` decl-macro,
// fed in via the `method_specs = [...]` couch! field.
//
// Hand-written installer used a plain JsObject + `constructor_native`
// attachment; switched here to the standard NativeFunction
// constructor path that Proxy / Iterator / Promise use.
otter_macros::couch! {
    name = "Date",
    feature = CORE,
    constructor = (length = 7, call = date_ctor_call),
    statics = {
        "now"   / 0 => date_now_call,
        "parse" / 1 => date_parse_call,
        "UTC"   / 7 => date_utc_call,
    },
    prototype = {
        method_specs = [
            crate::date::prototype::DATE_PROTOTYPE_METHODS,
            crate::date::prototype::DATE_PROTOTYPE_EXTRA_METHODS,
        ],
    },
}
