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
        ("isNaN", 1, number_is_nan_native),
        ("isFinite", 1, number_is_finite_native),
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
    for shared in ["parseInt", "parseFloat", "isNaN", "isFinite"] {
        if let Some(global_fn) = object::get(global, heap, shared) {
            // §21.1.2.{12,13} / §19.2.{4,5}: overwrite the static
            // with the global binding so identity holds. Configurable
            // so the redefine succeeds.
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
    let s = if let Some(arg) = args.first() {
        if let Some(s) = arg.as_string(ctx.heap()) {
            s.to_lossy_string(ctx.heap())
        } else {
            arg.display_string(ctx.heap())
        }
    } else {
        return Ok(Value::number(crate::number::NumberValue::from_f64(
            f64::NAN,
        )));
    };
    let radix = args
        .get(1)
        .and_then(|v| v.as_number())
        .map_or(0, |n| n.as_f64() as i32);
    Ok(Value::number(crate::number::parse::parse_int(&s, radix)))
}

fn number_parse_float_native(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let s = if let Some(arg) = args.first() {
        if let Some(s) = arg.as_string(ctx.heap()) {
            s.to_lossy_string(ctx.heap())
        } else {
            arg.display_string(ctx.heap())
        }
    } else {
        return Ok(Value::number(crate::number::NumberValue::from_f64(
            f64::NAN,
        )));
    };
    Ok(Value::number(crate::number::parse::parse_float(&s)))
}

// ---------------------------------------------------------------
// Legacy global wrappers — §19.2 / §B.2.1.
// ---------------------------------------------------------------

fn global_encode_uri(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::EncodeURI,
        args,
        ctx.heap_mut(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "encodeURI",
        reason: err.to_string(),
    })
}

fn global_encode_uri_component(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::EncodeURIComponent,
        args,
        ctx.heap_mut(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "encodeURIComponent",
        reason: err.to_string(),
    })
}

fn global_decode_uri(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::DecodeURI,
        args,
        ctx.heap_mut(),
    )
    .map_err(|err| match err {
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
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::DecodeURIComponent,
        args,
        ctx.heap_mut(),
    )
    .map_err(|err| match err {
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
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::Escape,
        args,
        ctx.heap_mut(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "escape",
        reason: err.to_string(),
    })
}

fn global_unescape(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    crate::global_functions::call(
        otter_bytecode::method_id::GlobalMethod::Unescape,
        args,
        ctx.heap_mut(),
    )
    .map_err(|err| NativeError::TypeError {
        name: "unescape",
        reason: err.to_string(),
    })
}

fn global_eval(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let arg = args.first().cloned().unwrap_or(Value::undefined());
    ctx.interp_mut()
        .run_eval(&arg, false)
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
