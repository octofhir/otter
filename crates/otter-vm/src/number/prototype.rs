//! `Number.prototype.*` intrinsic implementations.
//!
//! Wired through the same [`crate::intrinsics`] table the string and
//! array prototypes use, so `Op::CallMethodValue` reaches them via
//! the existing primitive-receiver dispatch path.
//!
//! # Contents
//! - [`NUMBER_PROTOTYPE_TABLE`] — declarative table built with the
//!   [`crate::intrinsics!`] macro.
//! - [`lookup`] — convenience accessor used by the dispatcher.
//! - One private `impl_*` function per method.
//!
//! # Foundation subset
//! - [`Number.prototype.toString(radix?)`](
//!     https://tc39.es/ecma262/#sec-number.prototype.tostring
//!   ) — integer values support full 2..=36 radix; floats only
//!   support radix 10 (matching the `display_string` rendering).
//! - [`Number.prototype.toFixed(digits)`](
//!     https://tc39.es/ecma262/#sec-number.prototype.tofixed
//!   ) — `digits` clamped to `0..=20`.

use super::NumberValue;
use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::js_surface::{Attr, MethodSpec};
use crate::string::JsString;
use crate::{NativeCall, NativeCtx, NativeError};

/// Coerce a digit-count argument (`fractionDigits` / `precision`)
/// per `ToIntegerOrInfinity` (§7.1.5). Surfaces the `Symbol` and
/// `BigInt` arms as `TypeError` (which the wrapper translates to
/// `IntrinsicError::BadArgument`); the rest go through the loose
/// numeric coercion.
fn coerce_digits_arg(
    arg: Option<&Value>,
    default_undefined: f64,
    heap: &otter_gc::GcHeap,
) -> Result<f64, IntrinsicError> {
    use super::parse::IntegerCoercion;
    match arg {
        None | Some(Value::Undefined) => Ok(default_undefined),
        Some(v) => match super::parse::to_integer_or_infinity_strict(v, heap) {
            IntegerCoercion::Ok(n) => Ok(n),
            IntegerCoercion::SymbolNotConvertible => Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "cannot convert a Symbol to a number",
            }),
            IntegerCoercion::BigIntNotConvertible => Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "cannot convert a BigInt to a number",
            }),
        },
    }
}

fn receiver_number(args: &IntrinsicArgs<'_>) -> Result<NumberValue, IntrinsicError> {
    match args.receiver {
        Value::Number(n) => Ok(*n),
        Value::Object(obj) => {
            // Per ECMA-262 `thisNumberValue`: if `this` has a
            // `[[NumberData]]` internal slot, use it.
            let gc = &*args.gc_heap;
            crate::object::number_data(*obj, gc)
                .ok_or(IntrinsicError::BadReceiver { expected: "number" })
        }
        _ => Err(IntrinsicError::BadReceiver { expected: "number" }),
    }
}

/// `Number.prototype.toString(radix = 10)`.
fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    // §21.1.3.6 step 2 — `radix` defaults to 10 when `undefined`,
    // otherwise routes through `ToIntegerOrInfinity`. Out-of-range
    // (`< 2` or `> 36`) raises RangeError. Symbol / BigInt raise
    // TypeError (mapped at the intrinsic error layer).
    let radix: u32 = match args.args.first() {
        None | Some(Value::Undefined) => 10,
        Some(Value::Symbol(_)) => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "Cannot convert a Symbol value to a number",
            });
        }
        Some(Value::BigInt(_)) => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "Cannot convert a BigInt value to a number",
            });
        }
        Some(other) => {
            let f = match other {
                Value::Number(n) => n.as_f64(),
                Value::Boolean(true) => 1.0,
                Value::Boolean(false) | Value::Null => 0.0,
                Value::String(s) => {
                    crate::number::parse::to_number_from_string(&s.to_lossy_string(args.gc_heap))
                        .as_f64()
                }
                _ => f64::NAN,
            };
            let trunc = if f.is_nan() { 0.0 } else { f.trunc() };
            if !trunc.is_finite() || !(2.0..=36.0).contains(&trunc) {
                return Err(IntrinsicError::OutOfRange {
                    index: 0,
                    reason: "must be an integer in 2..=36",
                });
            }
            trunc as u32
        }
    };
    if radix == 10 {
        return Ok(Value::string(super::ecma::number_to_string(
            recv.as_f64(),
            args.gc_heap,
        )?));
    }
    let rendered = super::dragon4::number_to_string_radix(recv.as_f64(), radix);
    Ok(Value::string(JsString::from_str(&rendered, args.gc_heap)?))
}

/// `Number.prototype.toFixed(digits = 0)`.
fn impl_to_fixed(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    // §21.1.3.3 step 2: `f = ToIntegerOrInfinity(fractionDigits)`.
    let f_arg = coerce_digits_arg(args.args.first(), 0.0, args.gc_heap)?;
    // §21.1.3.3 step 3: `f` outside `[0, 100]` (or `±Infinity`)
    // raises `RangeError`.
    if !f_arg.is_finite() || !(0.0..=100.0).contains(&f_arg) {
        return Err(IntrinsicError::OutOfRange {
            index: 0,
            reason: "must be an integer in 0..=100",
        });
    }
    let digits = f_arg as u32;
    let rendered = super::ecma_fixed::number_to_fixed(recv.as_f64(), digits);
    Ok(Value::string(JsString::from_latin1(
        rendered.as_bytes(),
        args.gc_heap,
    )?))
}

/// §21.1.3.2 `Number.prototype.toExponential(fractionDigits?)`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-number.prototype.toexponential>
fn impl_to_exponential(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    let value = recv.as_f64();
    // §21.1.3.2 step 3: non-finite returns ToString(x) regardless
    // of `fractionDigits` validity. Run the cold path BEFORE the
    // range check so `(Infinity).toExponential(101)` returns
    // `"Infinity"` (matching V8 / Test262 `infinity.js`).
    if !value.is_finite() {
        return Ok(Value::string(super::ecma::number_to_string(
            value,
            args.gc_heap,
        )?));
    }
    // §21.1.3.2 step 2: `f = undefined ? undefined :
    //   ToIntegerOrInfinity(fractionDigits)`.
    let digits: Option<u32> = match args.args.first() {
        None | Some(Value::Undefined) => None,
        Some(_) => {
            let f = coerce_digits_arg(args.args.first(), 0.0, args.gc_heap)?;
            // §21.1.3.2 step 6: out-of-range raises `RangeError`.
            if !f.is_finite() || !(0.0..=100.0).contains(&f) {
                return Err(IntrinsicError::OutOfRange {
                    index: 0,
                    reason: "must be an integer in 0..=100",
                });
            }
            Some(f as u32)
        }
    };
    let rendered = super::ecma_fixed::number_to_exponential(value, digits);
    Ok(Value::string(JsString::from_latin1(
        rendered.as_bytes(),
        args.gc_heap,
    )?))
}

/// §21.1.3.5 `Number.prototype.toPrecision(precision?)`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-number.prototype.toprecision>
fn impl_to_precision(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    let value = recv.as_f64();
    // §21.1.3.5 step 2: undefined precision is plain ToString.
    if matches!(args.args.first(), None | Some(Value::Undefined)) {
        return Ok(Value::string(super::ecma::number_to_string(
            value,
            args.gc_heap,
        )?));
    }
    // §21.1.3.5 step 3: `p = ToIntegerOrInfinity(precision)`. We
    // run this BEFORE the non-finite check so that a `Symbol` /
    // `BigInt` arg surfaces a `TypeError` and a throwing `valueOf`
    // propagates per §21.1.3.5 step 3 (matching test262
    // `nan.js` / `return-abrupt-tointeger-precision*` cases).
    let p = coerce_digits_arg(args.args.first(), 0.0, args.gc_heap)?;
    // §21.1.3.5 step 4: NaN/Infinity short-circuit AFTER coercion.
    if !value.is_finite() {
        return Ok(Value::string(super::ecma::number_to_string(
            value,
            args.gc_heap,
        )?));
    }
    // §21.1.3.5 step 5: out-of-range raises `RangeError`.
    if !p.is_finite() || !(1.0..=100.0).contains(&p) {
        return Err(IntrinsicError::OutOfRange {
            index: 0,
            reason: "must be an integer in 1..=100",
        });
    }
    let precision = p as u32;
    let rendered = super::ecma_fixed::number_to_precision(value, Some(precision));
    Ok(Value::string(JsString::from_latin1(
        rendered.as_bytes(),
        args.gc_heap,
    )?))
}

/// §21.1.3.7 `Number.prototype.valueOf()` — returns the receiver.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-number.prototype.valueof>
fn impl_value_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::number(receiver_number(args)?))
}

/// Declarative `Number.prototype` table.
pub static NUMBER_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Number,
            "toString"      / 1 => impl_to_string,
            "toFixed"       / 1 => impl_to_fixed,
            "toExponential" / 1 => impl_to_exponential,
            "toPrecision"   / 1 => impl_to_precision,
            "toLocaleString"/ 0 => impl_to_string,
            "valueOf"       / 0 => impl_value_of,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    NUMBER_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Number, name)
}

/// `MethodSpec` list installed on `Number.prototype` by
/// `bootstrap::install_number`. Each entry routes through the
/// shared [`NUMBER_PROTOTYPE_TABLE`] via a native callback so the
/// primitive-receiver fast path (`Op::CallMethodValue`) and the
/// object-property path (`Number.prototype.toString.call(...)`) end
/// up at the same implementation.
pub static NUMBER_PROTOTYPE_METHODS: &[MethodSpec] = &[
    method("toString", 1, native_to_string),
    method("toFixed", 1, native_to_fixed),
    method("toExponential", 1, native_to_exponential),
    method("toPrecision", 1, native_to_precision),
    method("toLocaleString", 0, native_to_locale_string),
    method("valueOf", 0, native_value_of),
];

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

fn native_number_method(
    name: &'static str,
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
) -> Result<Value, NativeError> {
    let receiver = *ctx.this_value();
    // §21.1.3.{3,4,5} — `toFixed` / `toExponential` / `toPrecision`
    // route their fractional-digits / precision argument through
    // `ToIntegerOrInfinity`, which itself starts with `ToNumber`.
    // For non-primitive args (`(123).toPrecision([2])`,
    // `(0).toFixed(new Number(3))`) ToNumber observes
    // `@@toPrimitive` / `valueOf` / `toString` via §7.1.1
    // ToPrimitive(number). Pre-coerce here so the intrinsic table
    // sees a primitive and surfaces RangeError / OK in line with
    // the spec ladder.
    let coerced: smallvec::SmallVec<[Value; 4]> =
        if matches!(name, "toFixed" | "toExponential" | "toPrecision") {
            let exec = ctx.execution_context().cloned();
            let mut out: smallvec::SmallVec<[Value; 4]> =
                smallvec::SmallVec::with_capacity(args.len());
            for arg in args {
                if matches!(
                    arg,
                    Value::Object(_)
                        | Value::Array(_)
                        | Value::Function { .. }
                        | Value::Closure(_)
                        | Value::NativeFunction(_)
                        | Value::BoundFunction(_)
                        | Value::ClassConstructor(_)
                        | Value::Proxy(_)
                        | Value::RegExp(_)
                ) {
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
                        Err(crate::VmError::Uncaught { value }) => {
                            return Err(NativeError::Thrown {
                                name,
                                message: value,
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
            out
        } else {
            args.iter().cloned().collect()
        };
    let allocation_roots = ctx.collect_native_roots();
    let entry = lookup(name).ok_or_else(|| NativeError::TypeError {
        name,
        reason: "unknown Number.prototype method".to_string(),
    })?;
    (entry.impl_fn)(&mut IntrinsicArgs {
        receiver: &receiver,
        args: &coerced,
        gc_heap: ctx.heap_mut(),
        allocation_roots: allocation_roots.as_slice(),
    })
    .map_err(|err| match err {
        // Preserve the spec error class when the intrinsic surfaces
        // an out-of-range argument so the JS-visible exception is a
        // `RangeError` (per ECMA-262 for the toFixed / toExponential
        // / toPrecision wrappers).
        IntrinsicError::OutOfRange { .. } => NativeError::RangeError {
            name,
            reason: err.to_string(),
        },
        _ => NativeError::TypeError {
            name,
            reason: err.to_string(),
        },
    })
}

fn native_to_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_number_method("toString", ctx, args)
}

fn native_to_fixed(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_number_method("toFixed", ctx, args)
}

fn native_to_exponential(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_number_method("toExponential", ctx, args)
}

fn native_to_precision(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_number_method("toPrecision", ctx, args)
}

fn native_to_locale_string(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_number_method("toLocaleString", ctx, args)
}

fn native_value_of(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    native_number_method("valueOf", ctx, args)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(
        recv: &'a Value,
        args: &'a [Value],
        gc_heap: &'a mut otter_gc::GcHeap,
    ) -> IntrinsicArgs<'a> {
        IntrinsicArgs {
            receiver: recv,
            args,
            gc_heap,
            allocation_roots: &[],
        }
    }

    #[test]
    fn to_string_default_radix_is_10() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::number(NumberValue::Smi(255));
        let entry = lookup("toString").unwrap();
        let out = (entry.impl_fn)(&mut args(&recv, &[], &mut gc_heap)).unwrap();
        assert_eq!(out.display_string(&gc_heap), "255");
    }

    #[test]
    fn to_string_hex_radix() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::number(NumberValue::Smi(255));
        let radix = Value::number(NumberValue::Smi(16));
        let entry = lookup("toString").unwrap();
        let out =
            (entry.impl_fn)(&mut args(&recv, std::slice::from_ref(&radix), &mut gc_heap)).unwrap();
        assert_eq!(out.display_string(&gc_heap), "ff");
    }

    #[test]
    fn to_fixed_two_decimals() {
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::number(NumberValue::Double(1.75));
        let two = Value::number(NumberValue::Smi(2));
        let entry = lookup("toFixed").unwrap();
        let out =
            (entry.impl_fn)(&mut args(&recv, std::slice::from_ref(&two), &mut gc_heap)).unwrap();
        assert_eq!(out.display_string(&gc_heap), "1.75");
    }
}
