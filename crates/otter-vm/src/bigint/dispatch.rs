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
use crate::{Value, VmError, oom_to_vm};
use num_bigint::BigInt;
use num_traits::Signed;

/// Dispatch `BigInt(...)` ([`BigIntMethod::Construct`]) /
/// `BigInt.<method>(...)`. Routes the typed [`BigIntMethod`]
/// emitted by the compiler.
///
/// # Errors
/// - [`VmError::TypeMismatch`] for wrong-shape arguments.
/// - [`VmError::OutOfMemory`] when the body allocation fails.
pub fn call(
    interp: &mut crate::Interpreter,
    method: otter_bytecode::method_id::BigIntMethod,
    args: &[Value],
) -> Result<Value, VmError> {
    use otter_bytecode::method_id::BigIntMethod as M;
    match method {
        // §21.2.1 BigInt(value) — coerce `value` to a BigInt.
        M::Construct => {
            let value = args.first().cloned().unwrap_or(Value::undefined());
            let big = to_bigint(interp, &value)?;
            let handle = BigIntValue::from_inner(interp.gc_heap_mut(), big).map_err(oom_to_vm)?;
            Ok(Value::big_int(handle))
        }
        // §21.2.2.1 BigInt.asIntN(bits, value).
        M::AsIntN => {
            let bits = expect_bits(args.first(), interp)?;
            let value = args.get(1).cloned().unwrap_or(Value::undefined());
            let n = to_bigint_strict(interp, &value)?;
            let clipped = as_int_n(bits, &n);
            let handle =
                BigIntValue::from_inner(interp.gc_heap_mut(), clipped).map_err(oom_to_vm)?;
            Ok(Value::big_int(handle))
        }
        // §21.2.2.2 BigInt.asUintN(bits, value).
        M::AsUintN => {
            let bits = expect_bits(args.first(), interp)?;
            let value = args.get(1).cloned().unwrap_or(Value::undefined());
            let n = to_bigint_strict(interp, &value)?;
            let clipped = as_uint_n(bits, &n);
            let handle =
                BigIntValue::from_inner(interp.gc_heap_mut(), clipped).map_err(oom_to_vm)?;
            Ok(Value::big_int(handle))
        }
    }
}

/// §7.1.13 ToBigInt — Number must be a safe integer; String parses
/// as integer literal; Boolean → 0n / 1n; BigInt passes through.
fn to_bigint(interp: &crate::Interpreter, value: &Value) -> Result<BigInt, VmError> {
    if let Some(b) = value.as_big_int() {
        return Ok(b.clone_inner(interp.gc_heap()));
    }
    if let Some(b) = value.as_boolean() {
        return Ok(BigInt::from(if b { 1 } else { 0 }));
    }
    // §21.2.1.1 step 3.a — `Number → NumberToBigInt`. The spec
    // throws **RangeError** on non-integer / non-finite values
    // and otherwise produces the matching integer.
    if let Some(n) = value.as_number() {
        let f = n.as_f64();
        if !f.is_finite() || f.fract() != 0.0 {
            return Err(interp
                .err_range(("The number is not a safe integer for BigInt".to_string()).into()));
        }
        return Ok(BigInt::from(f as i128));
    }
    if let Some(s) = value.as_string(interp.gc_heap()) {
        return string_to_bigint(interp, &s.to_lossy_string(interp.gc_heap()));
    }
    // §7.1.13 step 7 — Symbol → TypeError.
    if value.is_symbol() {
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a BigInt".to_string()).into())
        );
    }
    // §7.1.13 step 4 — ToPrimitive(value, "number") then
    // recursive ToBigInt. The caller (`bigint_ctor_call`) has
    // already run `coerce_bigint_call_args` so we should see a
    // primitive here. A remaining Object reaches the wildcard
    // and surfaces as TypeError.
    if value.is_array() {
        return Err(interp.err_type(("Cannot convert Array to a BigInt".to_string()).into()));
    }
    if value.is_null() || value.is_undefined() {
        return Err(
            interp.err_type(("Cannot convert null or undefined to a BigInt".to_string()).into())
        );
    }
    Err(interp.err_type(("Cannot convert value to a BigInt".to_string()).into()))
}

/// §7.1.13 ToBigInt — the strict conversion used by `BigInt.asIntN` /
/// `asUintN`. Unlike the `BigInt()` constructor's `NumberToBigInt`
/// (shared `to_bigint`), a Number operand is a `TypeError` here, not a
/// silent integer conversion. The operand is already ToPrimitive'd by
/// `coerce_bigint_call_args`, so a Number reaching this point came from
/// a numeric primitive or a `valueOf` / `@@toPrimitive` result.
fn to_bigint_strict(interp: &crate::Interpreter, value: &Value) -> Result<BigInt, VmError> {
    if value.is_number() {
        return Err(interp.err_type(("Cannot convert a Number to a BigInt".to_string()).into()));
    }
    to_bigint(interp, value)
}

fn string_to_bigint(interp: &crate::Interpreter, text: &str) -> Result<BigInt, VmError> {
    let trimmed = text.trim_matches(crate::number::parse::is_str_whitespace);
    // §7.1.14.1 StringToBigInt — empty string is 0n, accept decimal
    // / `0x` / `0o` / `0b` prefixes with optional leading sign on
    // the decimal form. Otherwise spec §21.2.1.1 step 3.b raises
    // **SyntaxError** (mapped through `vm_to_native`).
    if trimmed.is_empty() {
        return Ok(BigInt::from(0));
    }
    trimmed
        .parse::<BigInt>()
        .ok()
        .or_else(|| parse_radix_literal(trimmed))
        .ok_or_else(|| {
            interp.err_syntax((format!("Cannot convert {trimmed:?} to a BigInt")).into())
        })
}

fn parse_radix_literal(input: &str) -> Option<BigInt> {
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
    BigInt::parse_bytes(body.as_bytes(), radix)
}

/// §7.1.22 `ToIndex(arg)` — used by `BigInt.asIntN` /
/// `BigInt.asUintN`'s `bits` argument. Coerces through `ToNumber`
/// then `ToIntegerOrInfinity`, rejecting Symbol / BigInt with
/// **TypeError**, negatives and overflow with **RangeError**, and
/// returning `0` for NaN / undefined per the spec.
fn expect_bits(arg: Option<&Value>, interp: &crate::Interpreter) -> Result<u32, VmError> {
    let Some(v) = arg else {
        return Ok(0);
    };
    let n = if v.is_undefined() {
        return Ok(0);
    } else if let Some(num) = v.as_number() {
        num.as_f64()
    } else if v.is_null() {
        0.0
    } else if let Some(b) = v.as_boolean() {
        if b { 1.0 } else { 0.0 }
    } else if let Some(s) = v.as_string(interp.gc_heap()) {
        crate::number::parse::to_number_from_string(&s.to_lossy_string(interp.gc_heap())).as_f64()
    } else if v.is_symbol() {
        return Err(
            interp.err_type(("Cannot convert a Symbol value to a number".to_string()).into())
        );
    } else if v.is_big_int() {
        return Err(
            interp.err_type(("Cannot convert a BigInt value to a number".to_string()).into())
        );
    } else {
        // Object operands should have been pre-coerced by the
        // dispatcher's `coerce_bigint_call_args` ladder. Anything
        // else is treated as a non-primitive that fails the
        // ToNumber arm.
        return Err(interp.err_type(("Cannot convert value to a number".to_string()).into()));
    };
    // §7.1.5 ToIntegerOrInfinity — NaN collapses to 0, infinities
    // stay; §7.1.22 ToIndex then rejects negatives / overflows.
    if n.is_nan() {
        return Ok(0);
    }
    let trunc = n.trunc();
    if trunc.is_infinite() || !(0.0..=9_007_199_254_740_991.0).contains(&trunc) {
        return Err(interp
            .err_range(("Invalid bits parameter for BigInt.asIntN / asUintN".to_string()).into()));
    }
    if trunc > u32::MAX as f64 {
        // The spec allows up to 2^53-1, but the per-arm implementation
        // can only address up to `u32::MAX` bits before overflow.
        return Err(interp.err_range(("Bits parameter exceeds supported range".to_string()).into()));
    }
    Ok(trunc as u32)
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
