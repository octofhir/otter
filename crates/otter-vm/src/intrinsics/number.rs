//! `%Number%` constructor installer.
//!
//! Routes through `couch!`. The constructor itself coerces via the
//! shared `interp.number_for_number_ctor` (matches §21.1.1.1 — the
//! BigInt-to-f64 conversion + ToPrimitive ladder live there). The 8
//! §21.1.2 numeric constants ride the `static_constants` block; the
//! 6 §21.1.2.{3-9} static predicates / parsers ride the `statics`
//! block. The `post_install` hook wires the §19.2.{4,5} identity
//! aliasing onto the global object (e.g. `globalThis.parseInt ===
//! Number.parseInt`) plus the legacy `globalThis.eval` / `escape`
//! reflective bindings.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-number-objects>

use crate::js_surface::{Attr, JsSurfaceError, ObjectBuilder};
use crate::native_function::NativeCall;
use crate::object::{self, JsObject};
use crate::{NativeCtx, NativeError, Value, VmError};

otter_macros::couch! {
    name = "Number",
    feature = CORE,
    constructor = (length = 1, call = number_ctor_call),
    statics = {
        "isNaN"         / 1 => number_is_nan_native,
        "isFinite"      / 1 => number_is_finite_native,
        "isInteger"     / 1 => number_is_integer_native,
        "isSafeInteger" / 1 => number_is_safe_integer_native,
        "parseInt"      / 2 => number_parse_int_native,
        "parseFloat"    / 1 => number_parse_float_native,
    },
    static_constants = [
        ("MAX_VALUE",         Number(f64::MAX)),
        ("MIN_VALUE",         Number(5e-324)),
        ("EPSILON",           Number(f64::EPSILON)),
        ("MAX_SAFE_INTEGER",  Number(((1u64 << 53) - 1) as f64)),
        ("MIN_SAFE_INTEGER",  Number(-(((1u64 << 53) - 1) as f64))),
        ("POSITIVE_INFINITY", Number(f64::INFINITY)),
        ("NEGATIVE_INFINITY", Number(f64::NEG_INFINITY)),
        ("NaN",               Number(f64::NAN)),
    ],
    prototype = {
        method_specs = [crate::number::prototype::NUMBER_PROTOTYPE_METHODS],
    },
    post_install = pin_number_data_and_globals,
}

/// Post-bootstrap fixup:
/// - §21.1.3 — set `[[NumberData]] = +0` on the prototype so
///   `Number.prototype.valueOf()` / `toString()` recover the value.
/// - §19.2 / §B.2.1 — install the legacy global aliases on
///   `globalThis` (`parseInt`, `parseFloat`, `isNaN`, `isFinite`,
///   `encodeURI*`, `decodeURI*`, `escape`, `unescape`, `eval`).
/// - §21.1.2.{12,13} / §19.2.{4,5} — once the global aliases exist,
///   overwrite `Number.parseInt` / `Number.parseFloat` /
///   `Number.isNaN` / `Number.isFinite` to point at the SAME callable
///   so `Number.parseInt === parseInt` holds.
fn pin_number_data_and_globals(
    heap: &mut otter_gc::GcHeap,
    global: JsObject,
    ctor: crate::native_function::NativeFunction,
) -> Result<(), JsSurfaceError> {
    let descriptor = ctor
        .own_property_descriptor(heap, "prototype")
        .map_err(|_| JsSurfaceError::OutOfMemory)?;
    let prototype = match descriptor.and_then(|d| match d.kind {
        crate::object::DescriptorKind::Data { value } => value.as_object(),
        _ => None,
    }) {
        Some(p) => p,
        None => return Ok(()),
    };
    crate::object::set_number_data(prototype, heap, crate::number::NumberValue::from_i32(0));

    let global_root = Value::object(global);
    let global_methods: &[(&'static str, u8, crate::native_function::NativeFastFn)] = &[
        ("parseInt", 2, number_parse_int_native),
        ("parseFloat", 1, number_parse_float_native),
        ("isNaN", 1, global_is_nan),
        ("isFinite", 1, global_is_finite),
        ("encodeURI", 1, global_encode_uri),
        ("encodeURIComponent", 1, global_encode_uri_component),
        ("decodeURI", 1, global_decode_uri),
        ("decodeURIComponent", 1, global_decode_uri_component),
        ("escape", 1, global_escape),
        ("unescape", 1, global_unescape),
        ("eval", 1, global_eval),
    ];
    {
        let mut global_builder =
            ObjectBuilder::from_object_with_value_roots(heap, global, vec![global_root]);
        for (name, length, call) in global_methods {
            global_builder.method(
                name,
                *length,
                NativeCall::Static(*call),
                Attr::builtin_function(),
            )?;
        }
    }
    // §21.1.2.{12,13} / §19.2.{4,5}: `Number.parseInt === parseInt`
    // and `Number.parseFloat === parseFloat` are the SAME callable.
    // `Number.isNaN` / `Number.isFinite` are intentionally NOT aliased
    // — they are strict (§21.1.2.{2,3}) while the global `isNaN` /
    // `isFinite` coerce via ToNumber (§19.2.{2,3}).
    for shared in ["parseInt", "parseFloat"] {
        if let Some(global_fn) = object::get(global, heap, shared) {
            // Overwrite the static with the global binding so identity
            // holds. Configurable so the redefine succeeds.
            let desc = crate::object::PropertyDescriptor::data(global_fn, true, false, true);
            if !ctor.define_own_property(heap, shared, desc) {
                return Err(JsSurfaceError::DefinePropertyFailed(shared));
            }
        }
    }
    Ok(())
}

// ---------------------------------------------------------------
// Constructor body — §21.1.1.1.
// ---------------------------------------------------------------

fn number_ctor_call(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let value = if args.is_empty() {
        crate::number::NumberValue::from_i32(0)
    } else {
        let context = ctx
            .execution_context()
            .cloned()
            .ok_or_else(|| NativeError::TypeError {
                name: "Number",
                reason: "missing execution context".to_string(),
            })?;
        ctx.cx
            .interp
            .number_for_number_ctor(&context, &args[0])
            .map_err(|e| match e {
                crate::VmError::TypeError { message } => NativeError::TypeError {
                    name: "Number",
                    reason: message,
                },
                crate::VmError::Uncaught { value } => NativeError::Thrown {
                    name: "Number",
                    message: value,
                },
                other => NativeError::TypeError {
                    name: "Number",
                    reason: other.to_string(),
                },
            })?
    };
    if ctx.is_construct_call() {
        let this = *ctx.this_value();
        if let Some(obj) = this.as_object() {
            crate::object::set_number_data(obj, ctx.heap_mut(), value);
            Ok(Value::object(obj))
        } else {
            Err(NativeError::TypeError {
                name: "Number",
                reason: "expected object receiver in `new Number(...)`".to_string(),
            })
        }
    } else {
        Ok(Value::number(value))
    }
}

// ---------------------------------------------------------------
// Static predicates / parsers — §21.1.2.{3-9}.
// ---------------------------------------------------------------

fn number_is_nan_native(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let result = args
        .first()
        .and_then(|v| v.as_number())
        .is_some_and(|n| n.as_f64().is_nan());
    Ok(Value::boolean(result))
}

fn number_is_finite_native(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let result = args
        .first()
        .and_then(|v| v.as_number())
        .is_some_and(|n| n.as_f64().is_finite());
    Ok(Value::boolean(result))
}

fn number_is_integer_native(
    _ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let v = args.first().cloned().unwrap_or(Value::undefined());
    Ok(Value::boolean(crate::number::parse::is_integer(&v)))
}

fn number_is_safe_integer_native(
    _ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let v = args.first().cloned().unwrap_or(Value::undefined());
    Ok(Value::boolean(crate::number::parse::is_safe_integer(&v)))
}

fn number_parse_int_native(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // §19.2.5 step 1: `inputString = ? ToString(string)` runs first
    // and is observable through a user `toString`/`valueOf`/
    // `@@toPrimitive` override.
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let context = native_context(ctx, "parseInt")?;
    let s = ctx
        .cx
        .interp
        .coerce_to_string(&context, &arg)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "parseInt"))?;
    // §19.2.5 step 4: `R = ? ToInt32(radix)` — coerce the radix after
    // the string so a user `valueOf` on the radix fires in spec order.
    let radix = match args.get(1) {
        Some(radix) if !radix.is_undefined() => {
            let context = native_context(ctx, "parseInt")?;
            let num = ctx
                .cx
                .interp
                .coerce_to_number(&context, radix)
                .map_err(|e| crate::native_function::vm_to_native_error(e, "parseInt"))?;
            crate::number::bitwise::to_int32(num)
        }
        _ => 0,
    };
    Ok(Value::number(crate::number::parse::parse_int(&s, radix)))
}

fn number_parse_float_native(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    // §19.2.4 step 1: `inputString = ? ToString(string)` — observable.
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let context = native_context(ctx, "parseFloat")?;
    let s = ctx
        .cx
        .interp
        .coerce_to_string(&context, &arg)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "parseFloat"))?;
    Ok(Value::number(crate::number::parse::parse_float(&s)))
}

// ---------------------------------------------------------------
// Legacy global wrappers — §19.2 / §B.2.1.
// ---------------------------------------------------------------

/// Recover the active [`ExecutionContext`], or surface the spec
/// `TypeError` that the contextless paths cannot raise. Coercion
/// helpers below need it to re-enter the interpreter for user
/// `toString`/`valueOf`/`@@toPrimitive` overrides.
fn native_context(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
) -> Result<crate::ExecutionContext, NativeError> {
    ctx.execution_context()
        .cloned()
        .ok_or_else(|| NativeError::TypeError {
            name,
            reason: "missing execution context".to_string(),
        })
}

/// §7.1.1 `ToPrimitive(value, "string")` at the NativeCtx layer so a
/// user `toString`/`valueOf`/`@@toPrimitive` override fires before the
/// contextless §19.2.6 / §B.2.1 string algorithms run. Returns the
/// coerced primitive `Value`: a String operand passes through
/// unchanged so lone surrogates survive (those algorithms inspect raw
/// UTF-16 units), while non-String primitives are rendered downstream.
/// A Symbol operand raises the spec `TypeError`.
fn coerce_first_to_string_value(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    if arg.is_string() {
        return Ok(arg);
    }
    let context = native_context(ctx, name)?;
    let prim = ctx
        .cx
        .interp
        .coerce_to_primitive(&context, &arg, crate::abstract_ops::ToPrimitiveHint::String)
        .map_err(|e| crate::native_function::vm_to_native_error(e, name))?;
    if prim.is_symbol() {
        return Err(NativeError::TypeError {
            name,
            reason: "Cannot convert a Symbol value to a string".to_string(),
        });
    }
    Ok(prim)
}

fn global_encode_uri(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced = coerce_first_to_string_value(ctx, args, "encodeURI")?;
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::EncodeURI,
        &[coerced],
        ctx.heap_mut(),
    )
    .map_err(|err| match err {
        crate::VmError::URIError { message } => NativeError::URIError {
            name: "encodeURI",
            reason: message,
        },
        other => NativeError::TypeError {
            name: "encodeURI",
            reason: other.to_string(),
        },
    })
}

fn global_encode_uri_component(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let coerced = coerce_first_to_string_value(ctx, args, "encodeURIComponent")?;
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::EncodeURIComponent,
        &[coerced],
        ctx.heap_mut(),
    )
    .map_err(|err| match err {
        crate::VmError::URIError { message } => NativeError::URIError {
            name: "encodeURIComponent",
            reason: message,
        },
        other => NativeError::TypeError {
            name: "encodeURIComponent",
            reason: other.to_string(),
        },
    })
}

fn global_decode_uri(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced = coerce_first_to_string_value(ctx, args, "decodeURI")?;
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::DecodeURI,
        &[coerced],
        ctx.heap_mut(),
    )
    .map_err(|err| match err {
        crate::VmError::URIError { message } => NativeError::URIError {
            name: "decodeURI",
            reason: message,
        },
        crate::VmError::TypeError { message } => NativeError::TypeError {
            name: "decodeURI",
            reason: message,
        },
        other => NativeError::TypeError {
            name: "decodeURI",
            reason: other.to_string(),
        },
    })
}

fn global_decode_uri_component(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let coerced = coerce_first_to_string_value(ctx, args, "decodeURIComponent")?;
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::DecodeURIComponent,
        &[coerced],
        ctx.heap_mut(),
    )
    .map_err(|err| match err {
        crate::VmError::URIError { message } => NativeError::URIError {
            name: "decodeURIComponent",
            reason: message,
        },
        crate::VmError::TypeError { message } => NativeError::TypeError {
            name: "decodeURIComponent",
            reason: message,
        },
        other => NativeError::TypeError {
            name: "decodeURIComponent",
            reason: other.to_string(),
        },
    })
}

fn global_escape(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced = coerce_first_to_string_value(ctx, args, "escape")?;
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::Escape,
        &[coerced],
        ctx.heap_mut(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "escape",
        reason: err.to_string(),
    })
}

fn global_unescape(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let coerced = coerce_first_to_string_value(ctx, args, "unescape")?;
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::Unescape,
        &[coerced],
        ctx.heap_mut(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "unescape",
        reason: err.to_string(),
    })
}

/// §19.2.3 `isNaN(number)` — the **global** function coerces its
/// argument via `? ToNumber(number)` (firing user `valueOf` /
/// `@@toPrimitive`) before the strict NaN test, unlike the strict
/// `Number.isNaN` (§21.1.2.3) which performs no coercion.
fn global_is_nan(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let context = native_context(ctx, "isNaN")?;
    let num = ctx
        .cx
        .interp
        .coerce_to_number(&context, &arg)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "isNaN"))?;
    Ok(Value::boolean(num.as_f64().is_nan()))
}

/// §19.2.2 `isFinite(number)` — the **global** function coerces via
/// `? ToNumber(number)` before the strict finiteness test, unlike the
/// strict `Number.isFinite` (§21.1.2.2).
fn global_is_finite(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    let context = native_context(ctx, "isFinite")?;
    let num = ctx
        .cx
        .interp
        .coerce_to_number(&context, &arg)
        .map_err(|e| crate::native_function::vm_to_native_error(e, "isFinite"))?;
    Ok(Value::boolean(num.as_f64().is_finite()))
}

fn global_eval(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    // §19.2.1.1 indirect eval: sloppy, global variable environment —
    // no caller-scope restrictions apply.
    ctx.interp_mut()
        .run_eval(&arg, crate::EvalCompileOptions::default())
        .map_err(|err| match err {
            VmError::SyntaxError { message } => NativeError::SyntaxError {
                name: "eval",
                reason: message,
            },
            err => NativeError::TypeError {
                name: "eval",
                reason: err.to_string(),
            },
        })
}
