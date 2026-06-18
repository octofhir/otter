//! `Number.prototype.*` native implementations.
//!
//! Each method is a `fn(&mut NativeCtx, &[Value]) -> Result<Value,
//! NativeError>` installed on `Number.prototype` via the `Number`
//! `couch!` surface, so `Op::CallMethodValue` and the
//! `Number.prototype.toString.call(...)` property path reach the same
//! implementation with a re-entrant handle.
//!
//! # Contents
//! - [`NUMBER_PROTOTYPE_METHODS`] — native method specs installed on
//!   the global `Number.prototype`.
//! - One native per method, plus the `thisNumberValue` /
//!   digit-coercion helpers they share.
//!
//! # Foundation subset
//! - [`Number.prototype.toString(radix?)`](
//!     https://tc39.es/ecma262/#sec-number.prototype.tostring
//!   ) — integer values support full 2..=36 radix; floats only
//!   support radix 10 (matching the `display_string` rendering).
//! - [`Number.prototype.toFixed(digits)`](
//!     https://tc39.es/ecma262/#sec-number.prototype.tofixed
//!   ) — `digits` clamped to `0..=20`.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-number-prototype-object>

use super::NumberValue;
use crate::Value;
use crate::js_surface::{Attr, MethodSpec};
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError};

/// §21.1.3 `thisNumberValue(value)` — unwrap a primitive number or a
/// Number wrapper's `[[NumberData]]`; otherwise `TypeError`.
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

/// §21.1.3.{3,4,5} — pre-coerce object-like arguments through
/// `ToPrimitive(Number)` so a `@@toPrimitive` / `valueOf` / `toString`
/// on a `fractionDigits` / `precision` argument runs before the
/// numeric ladder. Primitive arguments pass through unchanged.
fn coerce_numeric_args(
    ctx: &mut NativeCtx<'_>,
    name: &'static str,
    args: &[Value],
) -> Result<smallvec::SmallVec<[Value; 4]>, NativeError> {
    let exec = ctx.execution_context().cloned();
    let mut out: smallvec::SmallVec<[Value; 4]> = smallvec::SmallVec::with_capacity(args.len());
    for arg in args {
        let object_like = arg.is_object()
            || arg.is_array()
            || arg.is_function()
            || arg.is_closure()
            || arg.is_native_function()
            || arg.is_bound_function()
            || arg.is_class_constructor()
            || arg.is_proxy()
            || arg.is_regexp();
        if object_like {
            let Some(exec) = &exec else {
                out.push(*arg);
                continue;
            };
            let interp = ctx.interp_mut();
            match interp.evaluate_to_primitive(
                exec,
                arg,
                crate::abstract_ops::ToPrimitiveHint::Number,
            ) {
                Ok(primitive) => out.push(primitive),
                Err(crate::VmError::Uncaught) => {
                    let value = match interp.take_error_detail() {
                        Some(crate::run_control::ErrorDetail::Uncaught(m)) => m,
                        _ => Default::default(),
                    };
                    return Err(NativeError::Thrown {
                        name,
                        message: value.into(),
                    });
                }
                Err(err) => {
                    return Err(NativeError::TypeError {
                        name,
                        reason: err.to_string(),
                    });
                }
            }
        } else {
            out.push(*arg);
        }
    }
    Ok(out)
}

/// Coerce a digit-count argument (`fractionDigits` / `precision`) per
/// `ToIntegerOrInfinity` (§7.1.5). `Symbol` / `BigInt` arms surface as
/// `TypeError`; the rest go through the loose numeric coercion.
fn coerce_digits_arg(
    arg: Option<&Value>,
    default_undefined: f64,
    heap: &otter_gc::GcHeap,
    name: &'static str,
) -> Result<f64, NativeError> {
    use super::parse::IntegerCoercion;
    let Some(v) = arg else {
        return Ok(default_undefined);
    };
    if v.is_undefined() {
        return Ok(default_undefined);
    }
    match super::parse::to_integer_or_infinity_strict(v, heap) {
        IntegerCoercion::Ok(n) => Ok(n),
        IntegerCoercion::SymbolNotConvertible => Err(NativeError::TypeError {
            name,
            reason: "cannot convert a Symbol to a number".to_string(),
        }),
        IntegerCoercion::BigIntNotConvertible => Err(NativeError::TypeError {
            name,
            reason: "cannot convert a BigInt to a number".to_string(),
        }),
    }
}

/// §21.1.3.6 `Number.prototype.toString(radix = 10)` and
/// `Number.prototype.toLocaleString` (locale-agnostic) share this body.
fn to_string_radix(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    name: &'static str,
) -> Result<Value, NativeError> {
    let recv = this_number_value(ctx, name)?;
    // §21.1.3.6 step 2 — `radix` defaults to 10 when `undefined`,
    // otherwise routes through `ToIntegerOrInfinity`. Out-of-range
    // (`< 2` or `> 36`) raises RangeError. Symbol / BigInt raise
    // TypeError.
    let radix: u32 = match args.first() {
        None => 10,
        Some(v) if v.is_undefined() => 10,
        Some(v) => {
            // §21.1.3.6 step 4 — radixNumber = ToIntegerOrInfinity(radix),
            // whose ToNumber runs the operand's `valueOf` / `@@toPrimitive`
            // (a poisoned one throws) and rejects Symbol / BigInt. The
            // earlier inline coercion dropped an object operand to NaN
            // without ever observing its `valueOf`.
            let v = *v;
            let number = {
                let (interp, exec) = ctx.interp_mut_and_context();
                let exec = exec.ok_or_else(|| NativeError::TypeError {
                    name,
                    reason: "missing execution context".to_string(),
                })?;
                let number_result = crate::coerce::to_number_or_throw(interp, &exec, &v);
                number_result
                    .map_err(|err| crate::native_function::vm_to_native_error(interp, err, name))?
            };
            let f = number.as_f64();
            let trunc = if f.is_nan() { 0.0 } else { f.trunc() };
            if !trunc.is_finite() || !(2.0..=36.0).contains(&trunc) {
                return Err(NativeError::RangeError {
                    name,
                    reason: "must be an integer in 2..=36".to_string(),
                });
            }
            trunc as u32
        }
    };
    if radix == 10 {
        return Ok(Value::string(super::ecma::number_to_string(
            recv.as_f64(),
            ctx.heap_mut(),
        )?));
    }
    let rendered = super::dragon4::number_to_string_radix(recv.as_f64(), radix);
    Ok(Value::string(JsString::from_str(
        &rendered,
        ctx.heap_mut(),
    )?))
}

fn number_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    to_string_radix(ctx, args, "Number.prototype.toString")
}

fn number_to_locale_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    to_string_radix(ctx, args, "Number.prototype.toLocaleString")
}

/// §21.1.3.3 `Number.prototype.toFixed(digits = 0)`.
fn number_to_fixed(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const NAME: &str = "Number.prototype.toFixed";
    let coerced = coerce_numeric_args(ctx, NAME, args)?;
    let recv = this_number_value(ctx, NAME)?;
    // §21.1.3.3 step 2: `f = ToIntegerOrInfinity(fractionDigits)`.
    let f_arg = coerce_digits_arg(coerced.first(), 0.0, ctx.heap(), NAME)?;
    // §21.1.3.3 step 3: `f` outside `[0, 100]` (or `±Infinity`)
    // raises `RangeError`.
    if !f_arg.is_finite() || !(0.0..=100.0).contains(&f_arg) {
        return Err(NativeError::RangeError {
            name: NAME,
            reason: "must be an integer in 0..=100".to_string(),
        });
    }
    let digits = f_arg as u32;
    let rendered = super::ecma_fixed::number_to_fixed(recv.as_f64(), digits);
    Ok(Value::string(JsString::from_latin1(
        rendered.as_bytes(),
        ctx.heap_mut(),
    )?))
}

/// §21.1.3.2 `Number.prototype.toExponential(fractionDigits?)`.
fn number_to_exponential(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const NAME: &str = "Number.prototype.toExponential";
    let coerced = coerce_numeric_args(ctx, NAME, args)?;
    let recv = this_number_value(ctx, NAME)?;
    let value = recv.as_f64();
    // §21.1.3.2 step 2: `f = ? ToIntegerOrInfinity(fractionDigits)` runs
    // BEFORE the non-finite return (step 3), so a Symbol / BigInt
    // `fractionDigits` raises a TypeError even for a NaN / Infinity
    // receiver.
    let f_value: Option<f64> = if coerced.first().is_none_or(|v| v.is_undefined()) {
        None
    } else {
        Some(coerce_digits_arg(coerced.first(), 0.0, ctx.heap(), NAME)?)
    };
    // §21.1.3.2 step 3: a non-finite receiver returns ToString(x)
    // regardless of `fractionDigits`'s range, so
    // `(Infinity).toExponential(101)` returns `"Infinity"` (the range
    // check below — step 5 — never runs).
    if !value.is_finite() {
        return Ok(Value::string(super::ecma::number_to_string(
            value,
            ctx.heap_mut(),
        )?));
    }
    // §21.1.3.2 step 5: `f` must be an integer in `[0, 100]`.
    let digits: Option<u32> = match f_value {
        None => None,
        Some(f) => {
            if !f.is_finite() || !(0.0..=100.0).contains(&f) {
                return Err(NativeError::RangeError {
                    name: NAME,
                    reason: "must be an integer in 0..=100".to_string(),
                });
            }
            Some(f as u32)
        }
    };
    let rendered = super::ecma_fixed::number_to_exponential(value, digits);
    Ok(Value::string(JsString::from_latin1(
        rendered.as_bytes(),
        ctx.heap_mut(),
    )?))
}

/// §21.1.3.5 `Number.prototype.toPrecision(precision?)`.
fn number_to_precision(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    const NAME: &str = "Number.prototype.toPrecision";
    let coerced = coerce_numeric_args(ctx, NAME, args)?;
    let recv = this_number_value(ctx, NAME)?;
    let value = recv.as_f64();
    // §21.1.3.5 step 2: undefined precision is plain ToString.
    if coerced.first().is_none_or(|v| v.is_undefined()) {
        return Ok(Value::string(super::ecma::number_to_string(
            value,
            ctx.heap_mut(),
        )?));
    }
    // §21.1.3.5 step 3: `p = ToIntegerOrInfinity(precision)`, run
    // BEFORE the non-finite check so a `Symbol` / `BigInt` arg surfaces
    // a `TypeError` and a throwing `valueOf` propagates.
    let p = coerce_digits_arg(coerced.first(), 0.0, ctx.heap(), NAME)?;
    // §21.1.3.5 step 4: NaN/Infinity short-circuit AFTER coercion.
    if !value.is_finite() {
        return Ok(Value::string(super::ecma::number_to_string(
            value,
            ctx.heap_mut(),
        )?));
    }
    // §21.1.3.5 step 5: out-of-range raises `RangeError`.
    if !p.is_finite() || !(1.0..=100.0).contains(&p) {
        return Err(NativeError::RangeError {
            name: NAME,
            reason: "must be an integer in 1..=100".to_string(),
        });
    }
    let precision = p as u32;
    let rendered = super::ecma_fixed::number_to_precision(value, Some(precision));
    Ok(Value::string(JsString::from_latin1(
        rendered.as_bytes(),
        ctx.heap_mut(),
    )?))
}

/// §21.1.3.7 `Number.prototype.valueOf()` — returns the receiver.
fn number_value_of(ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number(this_number_value(
        ctx,
        "Number.prototype.valueOf",
    )?))
}

/// `MethodSpec` list installed on `Number.prototype` by the `Number`
/// `couch!` surface.
pub static NUMBER_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 1, number_to_string),
    method("toFixed", 1, number_to_fixed),
    method("toExponential", 1, number_to_exponential),
    method("toPrecision", 1, number_to_precision),
    method("toLocaleString", 0, number_to_locale_string),
    method("valueOf", 0, number_value_of),
];

/// Whether `name` is an installed `Number.prototype` method.
#[must_use]
pub fn is_builtin_method(name: &str) -> bool {
    NUMBER_PROTOTYPE_METHODS.iter().any(|m| m.name == name)
}

const fn method(
    name: &'static str,
    length: u8,
    call: for<'rt> fn(&mut NativeCtx<'rt>, &[Value]) -> Result<Value, NativeError>,
) -> MethodSpec {
    MethodSpec {
        name,
        length,
        attrs: Attr::builtin_function(),
        call: NativeCall::Static(call),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn to_string_default_radix_is_10() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let out = super::super::ecma::number_to_string(255.0, &mut gc_heap).unwrap();
        assert_eq!(out.to_lossy_string(&gc_heap), "255");
    }

    #[test]
    fn to_string_hex_radix() {
        assert_eq!(
            super::super::dragon4::number_to_string_radix(255.0, 16),
            "ff"
        );
    }

    #[test]
    fn to_fixed_two_decimals() {
        assert_eq!(super::super::ecma_fixed::number_to_fixed(1.75, 2), "1.75");
    }
}
