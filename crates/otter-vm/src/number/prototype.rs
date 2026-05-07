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
use crate::string::JsString;

fn receiver_number(args: &IntrinsicArgs<'_>) -> Result<NumberValue, IntrinsicError> {
    match args.receiver {
        Value::Number(n) => Ok(*n),
        _ => Err(IntrinsicError::BadReceiver { expected: "number" }),
    }
}

/// `Number.prototype.toString(radix = 10)`.
fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    let radix: u32 = match args.args.first() {
        None | Some(Value::Undefined) => 10,
        Some(Value::Number(n)) => {
            let r = n.as_f64();
            if !r.is_finite() || !(2.0..=36.0).contains(&r) || r.fract() != 0.0 {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be an integer in 2..=36",
                });
            }
            r as u32
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a number",
            });
        }
    };
    let rendered = if radix == 10 {
        recv.to_display_string()
    } else {
        match recv {
            NumberValue::Smi(n) => to_string_radix_i32(n, radix),
            NumberValue::Double(d) => {
                if d.is_nan() {
                    "NaN".to_string()
                } else if d.is_infinite() {
                    if d.is_sign_positive() {
                        "Infinity".to_string()
                    } else {
                        "-Infinity".to_string()
                    }
                } else if d.fract() == 0.0 && (i64::MIN as f64..=i64::MAX as f64).contains(&d) {
                    to_string_radix_i64(d as i64, radix)
                } else {
                    // Foundation slice doesn't ship a fractional
                    // radix renderer; fall back to base-10 so the
                    // call doesn't blow up on the rare path.
                    recv.to_display_string()
                }
            }
        }
    };
    Ok(Value::String(JsString::from_str(
        &rendered,
        args.string_heap,
    )?))
}

/// `Number.prototype.toFixed(digits = 0)`.
fn impl_to_fixed(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    let digits: usize = match args.args.first() {
        None | Some(Value::Undefined) => 0,
        Some(Value::Number(n)) => {
            let f = n.as_f64();
            if !f.is_finite() || !(0.0..=20.0).contains(&f) || f.fract() != 0.0 {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be an integer in 0..=20",
                });
            }
            f as usize
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a number",
            });
        }
    };
    let rendered = match recv {
        NumberValue::Double(d) if d.is_nan() => "NaN".to_string(),
        NumberValue::Double(d) if d.is_infinite() => {
            if d.is_sign_positive() {
                "Infinity".to_string()
            } else {
                "-Infinity".to_string()
            }
        }
        _ => format!("{:.*}", digits, recv.as_f64()),
    };
    Ok(Value::String(JsString::from_str(
        &rendered,
        args.string_heap,
    )?))
}

fn to_string_radix_i32(value: i32, radix: u32) -> String {
    to_string_radix_i64(i64::from(value), radix)
}

fn to_string_radix_i64(value: i64, radix: u32) -> String {
    if value == 0 {
        return "0".to_string();
    }
    let negative = value < 0;
    let mut n = value.unsigned_abs();
    let mut buf = Vec::with_capacity(8);
    while n > 0 {
        let digit = (n % u64::from(radix)) as u32;
        buf.push(char::from_digit(digit, radix).expect("radix in range"));
        n /= u64::from(radix);
    }
    if negative {
        buf.push('-');
    }
    buf.iter().rev().collect()
}

/// §21.1.3.3 `Number.prototype.toExponential(fractionDigits?)`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-number.prototype.toexponential>
fn impl_to_exponential(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    let value = recv.as_f64();
    if value.is_nan() {
        return Ok(Value::String(JsString::from_str("NaN", args.string_heap)?));
    }
    if value.is_infinite() {
        let s = if value.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        };
        return Ok(Value::String(JsString::from_str(s, args.string_heap)?));
    }
    let digits = match args.args.first() {
        None | Some(Value::Undefined) => None,
        Some(Value::Number(n)) => {
            let f = n.as_f64();
            if !f.is_finite() || !(0.0..=100.0).contains(&f) || f.fract() != 0.0 {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be an integer in 0..=100",
                });
            }
            Some(f as usize)
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a number",
            });
        }
    };
    let formatted = match digits {
        Some(d) => format!("{value:.*e}", d),
        None => format!("{value:e}"),
    };
    // Rust's `{:e}` emits `1e2`; ECMA-262 wants `1e+2`. Normalise
    // the exponent sign so output matches spec.
    let normalised = normalise_exp(&formatted);
    Ok(Value::String(JsString::from_str(
        &normalised,
        args.string_heap,
    )?))
}

/// §21.1.3.5 `Number.prototype.toPrecision(precision?)`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-number.prototype.toprecision>
fn impl_to_precision(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_number(args)?;
    if matches!(args.args.first(), None | Some(Value::Undefined)) {
        // No-precision form is equivalent to ToString.
        return Ok(Value::String(JsString::from_str(
            &recv.to_display_string(),
            args.string_heap,
        )?));
    }
    let value = recv.as_f64();
    if value.is_nan() {
        return Ok(Value::String(JsString::from_str("NaN", args.string_heap)?));
    }
    if value.is_infinite() {
        let s = if value.is_sign_positive() {
            "Infinity"
        } else {
            "-Infinity"
        };
        return Ok(Value::String(JsString::from_str(s, args.string_heap)?));
    }
    let precision = match args.args.first() {
        Some(Value::Number(n)) => {
            let f = n.as_f64();
            if !f.is_finite() || !(1.0..=100.0).contains(&f) || f.fract() != 0.0 {
                return Err(IntrinsicError::BadArgument {
                    index: 0,
                    reason: "must be an integer in 1..=100",
                });
            }
            f as usize
        }
        _ => {
            return Err(IntrinsicError::BadArgument {
                index: 0,
                reason: "must be a number",
            });
        }
    };
    // §21.1.3.5 step 11 — choose between fixed-decimal and
    // exponential rendering based on the magnitude vs. precision.
    let abs = value.abs();
    let exponent = if abs == 0.0 {
        0
    } else {
        abs.log10().floor() as i32
    };
    let rendered = if exponent < -6 || exponent >= precision as i32 {
        normalise_exp(&format!("{value:.*e}", precision - 1))
    } else {
        let after_decimal = (precision as i32 - 1 - exponent).max(0) as usize;
        format!("{value:.*}", after_decimal)
    };
    Ok(Value::String(JsString::from_str(
        &rendered,
        args.string_heap,
    )?))
}

/// §21.1.3.7 `Number.prototype.valueOf()` — returns the receiver.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-number.prototype.valueof>
fn impl_value_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::Number(receiver_number(args)?))
}

/// Rust's `{:e}` formatter emits `1e2` for positive exponents;
/// ECMA-262 §21.1.3.3 requires an explicit `+` sign. Mirror the
/// spec rendering by walking the exponent suffix and inserting `+`
/// when the exponent has no sign.
fn normalise_exp(raw: &str) -> String {
    if let Some(idx) = raw.find('e') {
        let (mantissa, exp) = raw.split_at(idx);
        let exp_body = &exp[1..];
        if exp_body.starts_with('+') || exp_body.starts_with('-') {
            return raw.to_string();
        }
        return format!("{mantissa}e+{exp_body}");
    }
    raw.to_string()
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
            "valueOf"       / 0 => impl_value_of,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    NUMBER_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Number, name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::string::StringHeap;

    fn args<'a>(
        recv: &'a Value,
        args: &'a [Value],
        heap: &'a StringHeap,
        gc_heap: &'a mut otter_gc::GcHeap,
    ) -> IntrinsicArgs<'a> {
        IntrinsicArgs {
            receiver: recv,
            args,
            string_heap: heap,
            gc_heap: std::cell::RefCell::new(gc_heap),
        }
    }

    #[test]
    fn to_string_default_radix_is_10() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::Number(NumberValue::Smi(255));
        let entry = lookup("toString").unwrap();
        let out = (entry.impl_fn)(&args(&recv, &[], &heap, &mut gc_heap)).unwrap();
        assert_eq!(out.display_string(), "255");
    }

    #[test]
    fn to_string_hex_radix() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        let recv = Value::Number(NumberValue::Smi(255));
        let radix = Value::Number(NumberValue::Smi(16));
        let entry = lookup("toString").unwrap();
        let out = (entry.impl_fn)(&args(
            &recv,
            std::slice::from_ref(&radix),
            &heap,
            &mut gc_heap,
        ))
        .unwrap();
        assert_eq!(out.display_string(), "ff");
    }

    #[test]
    fn to_fixed_two_decimals() {
        let heap = StringHeap::default();
        let mut gc_heap = otter_gc::GcHeap::new().expect("gc heap");
        // 3.5 / 2.0 = 1.75 — pick a value that doesn't trip the
        // `approx_constant` lint while still proving fixed-decimal
        // formatting end-to-end.
        let recv = Value::Number(NumberValue::Double(1.75));
        let two = Value::Number(NumberValue::Smi(2));
        let entry = lookup("toFixed").unwrap();
        let out = (entry.impl_fn)(&args(
            &recv,
            std::slice::from_ref(&two),
            &heap,
            &mut gc_heap,
        ))
        .unwrap();
        assert_eq!(out.display_string(), "1.75");
    }
}
