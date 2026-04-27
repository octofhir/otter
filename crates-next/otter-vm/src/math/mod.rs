//! `Math` namespace — constants and unary / variadic numeric
//! functions reachable through the dedicated `Op::MathLoad` and
//! `Op::MathCall` opcodes.
//!
//! Foundation goal: the compiler intercepts `Math.<name>` directly
//! so the runtime does not need a true global object yet. Each
//! entry below is registered in one of two static tables — one
//! for read-only constants (`PI`, `E`, …) and one for callable
//! routines (`abs`, `min`, …) — and looked up by name.
//!
//! # Contents
//! - [`load_constant`] — used by `Op::MathLoad`.
//! - [`call`] — used by `Op::MathCall`.
//! - [`MathError`] — failure modes the dispatcher converts to
//!   `VmError`.
//!
//! # See also
//! - [`docs/new-engine/tasks/28-bitwise-and-number-prototype.md`](
//!     ../../../docs/new-engine/tasks/28-bitwise-and-number-prototype.md
//!   )

use crate::Value;
use crate::number::{NumberValue, bitwise};

/// Foundation `Math` constants. Each constant is a static `f64` so
/// the compiler can also fold them at intern time later if it
/// wants to skip the runtime hop.
pub const PI: f64 = std::f64::consts::PI;
/// Base of the natural logarithm.
pub const E: f64 = std::f64::consts::E;

/// Failure modes for [`call`].
#[derive(Debug, Clone, thiserror::Error)]
#[non_exhaustive]
pub enum MathError {
    /// `name` is not a registered Math.* function or constant.
    #[error("Math.{0} is not defined")]
    UnknownMember(String),
    /// Argument is the wrong type or out of range.
    #[error("Math.{name} argument {index} {reason}")]
    BadArgument {
        /// Function name.
        name: &'static str,
        /// Argument index (0-based).
        index: u16,
        /// Short reason.
        reason: &'static str,
    },
}

/// Read a constant or other read-only Math property by name.
/// Returns `None` for unknown names so the dispatcher can surface
/// a uniform `UnknownMember` diagnostic.
#[must_use]
pub fn load_constant(name: &str) -> Option<Value> {
    match name {
        "PI" => Some(Value::Number(NumberValue::from_f64(PI))),
        "E" => Some(Value::Number(NumberValue::from_f64(E))),
        _ => None,
    }
}

/// Dispatch a `Math.<name>(args...)` call. Returns the result
/// value or a [`MathError`] the caller maps to `VmError`.
pub fn call(name: &str, args: &[Value]) -> Result<Value, MathError> {
    let entry = FUNCTIONS
        .iter()
        .find(|f| f.name == name)
        .ok_or_else(|| MathError::UnknownMember(name.to_string()))?;
    let nums = coerce_all(entry.name, args)?;
    Ok(Value::Number((entry.impl_fn)(&nums)))
}

/// One entry in the Math function table.
struct MathFn {
    name: &'static str,
    impl_fn: fn(&[NumberValue]) -> NumberValue,
}

fn coerce_all(name: &'static str, args: &[Value]) -> Result<Vec<NumberValue>, MathError> {
    let mut out = Vec::with_capacity(args.len());
    for (idx, v) in args.iter().enumerate() {
        let n = match v {
            Value::Number(n) => *n,
            Value::Boolean(true) => NumberValue::Smi(1),
            Value::Boolean(false) | Value::Null => NumberValue::Smi(0),
            Value::Undefined => NumberValue::Double(f64::NAN),
            _ => {
                return Err(MathError::BadArgument {
                    name,
                    index: idx as u16,
                    reason: "must be a number",
                });
            }
        };
        out.push(n);
    }
    Ok(out)
}

fn impl_abs(args: &[NumberValue]) -> NumberValue {
    let f = first_or_nan(args);
    NumberValue::Double(f.as_f64().abs()).canonicalize()
}

fn impl_floor(args: &[NumberValue]) -> NumberValue {
    NumberValue::Double(first_or_nan(args).as_f64().floor()).canonicalize()
}

fn impl_ceil(args: &[NumberValue]) -> NumberValue {
    NumberValue::Double(first_or_nan(args).as_f64().ceil()).canonicalize()
}

fn impl_round(args: &[NumberValue]) -> NumberValue {
    let f = first_or_nan(args).as_f64();
    // ES spec: half-to-positive-infinity (so .5 rounds up, -.5
    // rounds toward zero). Rust's `f64::round` rounds half-away-
    // from-zero, which differs for negative half-integers.
    let rounded = if f.is_nan() {
        f64::NAN
    } else if f.is_infinite() {
        f
    } else {
        (f + 0.5).floor()
    };
    NumberValue::Double(rounded).canonicalize()
}

fn impl_trunc(args: &[NumberValue]) -> NumberValue {
    NumberValue::Double(first_or_nan(args).as_f64().trunc()).canonicalize()
}

fn impl_sqrt(args: &[NumberValue]) -> NumberValue {
    NumberValue::Double(first_or_nan(args).as_f64().sqrt()).canonicalize()
}

fn impl_pow(args: &[NumberValue]) -> NumberValue {
    let base = args
        .first()
        .copied()
        .unwrap_or(NumberValue::Double(f64::NAN));
    let exp = args
        .get(1)
        .copied()
        .unwrap_or(NumberValue::Double(f64::NAN));
    bitwise::pow(base, exp)
}

fn impl_min(args: &[NumberValue]) -> NumberValue {
    if args.is_empty() {
        return NumberValue::Double(f64::INFINITY);
    }
    let mut current = args[0].as_f64();
    for v in &args[1..] {
        let f = v.as_f64();
        if f.is_nan() {
            return NumberValue::Double(f64::NAN);
        }
        // Per spec: -0 < +0.
        if f < current || (f == 0.0 && current == 0.0 && f.is_sign_negative()) {
            current = f;
        }
    }
    NumberValue::Double(current).canonicalize()
}

fn impl_max(args: &[NumberValue]) -> NumberValue {
    if args.is_empty() {
        return NumberValue::Double(f64::NEG_INFINITY);
    }
    let mut current = args[0].as_f64();
    for v in &args[1..] {
        let f = v.as_f64();
        if f.is_nan() {
            return NumberValue::Double(f64::NAN);
        }
        if f > current || (f == 0.0 && current == 0.0 && current.is_sign_negative()) {
            current = f;
        }
    }
    NumberValue::Double(current).canonicalize()
}

fn first_or_nan(args: &[NumberValue]) -> NumberValue {
    args.first()
        .copied()
        .unwrap_or(NumberValue::Double(f64::NAN))
}

const FUNCTIONS: &[MathFn] = &[
    MathFn {
        name: "abs",
        impl_fn: impl_abs,
    },
    MathFn {
        name: "floor",
        impl_fn: impl_floor,
    },
    MathFn {
        name: "ceil",
        impl_fn: impl_ceil,
    },
    MathFn {
        name: "round",
        impl_fn: impl_round,
    },
    MathFn {
        name: "trunc",
        impl_fn: impl_trunc,
    },
    MathFn {
        name: "sqrt",
        impl_fn: impl_sqrt,
    },
    MathFn {
        name: "pow",
        impl_fn: impl_pow,
    },
    MathFn {
        name: "min",
        impl_fn: impl_min,
    },
    MathFn {
        name: "max",
        impl_fn: impl_max,
    },
];

#[cfg(test)]
mod tests {
    use super::*;

    fn n(v: i32) -> Value {
        Value::Number(NumberValue::Smi(v))
    }

    #[test]
    fn constants_resolve() {
        let pi = load_constant("PI").unwrap();
        assert!((pi.as_number().unwrap().as_f64() - PI).abs() < 1e-12);
        assert!(load_constant("nope").is_none());
    }

    #[test]
    fn min_and_max_handle_nan() {
        let r = call("min", &[n(1), n(2), n(3)]).unwrap();
        assert_eq!(r.as_number().unwrap().as_smi(), Some(1));
        let r = call("max", &[n(1), n(2), n(3)]).unwrap();
        assert_eq!(r.as_number().unwrap().as_smi(), Some(3));
        let nan_r = call("max", &[n(1), Value::Number(NumberValue::Double(f64::NAN))]).unwrap();
        assert!(nan_r.as_number().unwrap().is_nan());
    }

    #[test]
    fn pow_routes_to_bitwise_pow() {
        let r = call("pow", &[n(2), n(10)]).unwrap();
        assert_eq!(r.as_number().unwrap().as_smi(), Some(1024));
    }
}
