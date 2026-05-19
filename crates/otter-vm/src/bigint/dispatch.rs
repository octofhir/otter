//! `BigInt.<static>` dispatcher — `BigInt(value)` constructor +
//! `BigInt.asIntN` / `BigInt.asUintN` static helpers. Routed
//! through [`crate::otter_bytecode::Op::BigIntCall`] by the
//! compiler.
//!
//! # Contents
//! - [`call`] — single entry point used by the dispatch loop.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-bigint-constructor>
//! - <https://tc39.es/ecma262/#sec-bigint.asintn>
//! - <https://tc39.es/ecma262/#sec-bigint.asuintn>
//! - <https://tc39.es/ecma262/#sec-tobigint>

use super::BigIntValue;
use crate::{Value, VmError};
use num_bigint::BigInt;
use num_traits::Signed;

/// Dispatch `BigInt(...)` ([`BigIntMethod::Construct`]) /
/// `BigInt.<method>(...)`. Routes the typed [`BigIntMethod`]
/// emitted by the compiler.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for wrong-shape arguments.
pub fn call(
    method: otter_bytecode::method_id::BigIntMethod,
    args: &[Value],
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::BigIntMethod as M;
    match method {
        // §21.2.1 BigInt(value) — coerce `value` to a BigInt.
        M::Construct => {
            let value = args.first().cloned().unwrap_or(Value::Undefined);
            Ok(Value::BigInt(to_bigint(&value)?))
        }
        // §21.2.2.1 BigInt.asIntN(bits, value).
        M::AsIntN => {
            let bits = expect_bits(args.first())?;
            let value = args.get(1).cloned().unwrap_or(Value::Undefined);
            let n = to_bigint(&value)?;
            Ok(Value::BigInt(BigIntValue::from_inner(as_int_n(
                bits,
                n.as_inner(),
            ))))
        }
        // §21.2.2.2 BigInt.asUintN(bits, value).
        M::AsUintN => {
            let bits = expect_bits(args.first())?;
            let value = args.get(1).cloned().unwrap_or(Value::Undefined);
            let n = to_bigint(&value)?;
            Ok(Value::BigInt(BigIntValue::from_inner(as_uint_n(
                bits,
                n.as_inner(),
            ))))
        }
    }
}

/// §7.1.13 ToBigInt — Number must be a safe integer; String parses
/// as integer literal; Boolean → 0n / 1n; BigInt passes through.
fn to_bigint(value: &Value) -> Result<BigIntValue, VmError> {
    match value {
        Value::BigInt(b) => Ok(b.clone()),
        Value::Boolean(true) => Ok(BigIntValue::from_i32(1)),
        Value::Boolean(false) => Ok(BigIntValue::from_i32(0)),
        // §21.2.1.1 step 3.a — `Number → NumberToBigInt`. The spec
        // throws **RangeError** on non-integer / non-finite values
        // and otherwise produces the matching integer.
        Value::Number(n) => {
            let f = n.as_f64();
            if !f.is_finite() || f.fract() != 0.0 {
                return Err(VmError::RangeError {
                    message: "The number is not a safe integer for BigInt".to_string(),
                });
            }
            Ok(BigIntValue::from_inner(BigInt::from(f as i128)))
        }
        Value::String(s) => string_to_bigint(&s.to_lossy_string()),
        // §7.1.13 step 7 — Symbol → TypeError.
        Value::Symbol(_) => Err(VmError::TypeError {
            message: "Cannot convert a Symbol value to a BigInt".to_string(),
        }),
        // §7.1.13 step 4 — ToPrimitive(value, "number") then
        // recursive ToBigInt. The caller (`bigint_ctor_call`) has
        // already run `coerce_bigint_call_args` so we should see a
        // primitive here. A remaining Object reaches the wildcard
        // and surfaces as TypeError.
        Value::Array(_) => Err(VmError::TypeError {
            message: "Cannot convert Array to a BigInt".to_string(),
        }),
        Value::Null | Value::Undefined => Err(VmError::TypeError {
            message: "Cannot convert null or undefined to a BigInt".to_string(),
        }),
        _ => Err(VmError::TypeError {
            message: "Cannot convert value to a BigInt".to_string(),
        }),
    }
}

fn string_to_bigint(text: &str) -> Result<BigIntValue, VmError> {
    let trimmed = text.trim();
    // §7.1.14.1 StringToBigInt — empty string is 0n, accept decimal
    // / `0x` / `0o` / `0b` prefixes with optional leading sign on
    // the decimal form. Otherwise spec §21.2.1.1 step 3.b raises
    // **SyntaxError** (mapped through `vm_to_native`).
    if trimmed.is_empty() {
        return Ok(BigIntValue::from_i32(0));
    }
    BigIntValue::from_decimal(trimmed)
        .or_else(|| parse_radix_literal(trimmed))
        .ok_or_else(|| VmError::SyntaxError {
            message: format!("Cannot convert {trimmed:?} to a BigInt"),
        })
}

fn parse_radix_literal(input: &str) -> Option<BigIntValue> {
    if input.len() < 3 {
        return None;
    }
    let lower = input.to_ascii_lowercase();
    let (radix, body) = if let Some(rest) = lower.strip_prefix("0x") {
        (16u32, rest)
    } else if let Some(rest) = lower.strip_prefix("0o") {
        (8u32, rest)
    } else if let Some(rest) = lower.strip_prefix("0b") {
        (2u32, rest)
    } else {
        return None;
    };
    BigInt::parse_bytes(body.as_bytes(), radix).map(BigIntValue::from_inner)
}

fn expect_bits(arg: Option<&Value>) -> Result<u32, VmError> {
    let n = match arg {
        Some(Value::Number(n)) => n.as_f64(),
        _ => return Err(VmError::TypeMismatch),
    };
    if !n.is_finite() || n < 0.0 || n.fract() != 0.0 || n > u32::MAX as f64 {
        return Err(VmError::TypeMismatch);
    }
    Ok(n as u32)
}

/// §21.2.2.1 BigInt.asIntN — clip `value` to a signed N-bit
/// integer. Result is in `[-2^(N-1), 2^(N-1) - 1]`.
fn as_int_n(bits: u32, value: &BigInt) -> BigInt {
    if bits == 0 {
        return BigInt::from(0);
    }
    let modulus = BigInt::from(1u32) << bits;
    let half = BigInt::from(1u32) << (bits - 1);
    let mut wrapped = value.modpow(&BigInt::from(1u32), &modulus);
    if wrapped.is_negative() {
        wrapped += &modulus;
    }
    if wrapped >= half {
        wrapped - modulus
    } else {
        wrapped
    }
}

/// §21.2.2.2 BigInt.asUintN — clip `value` to an unsigned N-bit
/// integer. Result is in `[0, 2^N - 1]`.
fn as_uint_n(bits: u32, value: &BigInt) -> BigInt {
    if bits == 0 {
        return BigInt::from(0);
    }
    let modulus = BigInt::from(1u32) << bits;
    let mut wrapped = value % &modulus;
    if wrapped.is_negative() {
        wrapped += &modulus;
    }
    wrapped
}
