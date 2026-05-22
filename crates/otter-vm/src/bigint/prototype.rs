//! `BigInt.prototype.*` intrinsic implementations.
//!
//! Wired through the same [`crate::intrinsics`] table the string
//! and number prototypes use, so `Op::CallMethodValue` reaches them
//! via the existing primitive-receiver dispatch path.
//!
//! # Contents
//! - [`BIGINT_PROTOTYPE_TABLE`] — declarative table built with the
//!   [`crate::intrinsics!`] macro.
//! - [`lookup`] — convenience accessor used by the dispatcher.
//! - One private `impl_*` function per method.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-bigint-prototype-object>

use super::BigIntValue;
use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::string::JsString;

fn receiver_bigint(args: &IntrinsicArgs<'_>) -> Result<BigIntValue, IntrinsicError> {
    args.receiver
        .as_big_int()
        .ok_or(IntrinsicError::BadReceiver { expected: "bigint" })
}

/// §21.2.3.4 `BigInt.prototype.toString(radix = 10)`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-bigint.prototype.tostring>
fn impl_to_string(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_bigint(args)?;
    // §21.2.3.4 step 2 — `radix` flows through
    // `ToIntegerOrInfinity`; values outside `[2, 36]` raise
    // RangeError. Symbol / BigInt raise TypeError (handled by the
    // surrounding intrinsic-dispatch error mapping).
    let Some(first) = args.args.first() else {
        return Ok(Value::string(JsString::from_str(
            &recv.to_decimal_string(args.gc_heap),
            args.gc_heap,
        )?));
    };
    let radix: u32 = if first.is_undefined() {
        10
    } else if first.is_symbol() {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "Cannot convert a Symbol value to a number",
        });
    } else if first.is_big_int() {
        return Err(IntrinsicError::BadArgument {
            index: 0,
            reason: "Cannot convert a BigInt value to a number",
        });
    } else {
        let other = first;
        let f = if let Some(n) = other.as_number() {
            n.as_f64()
        } else if let Some(b) = other.as_boolean() {
            if b { 1.0 } else { 0.0 }
        } else if other.is_null() {
            0.0
        } else if let Some(s) = other.as_string() {
            crate::number::parse::to_number_from_string(&s.to_lossy_string(args.gc_heap)).as_f64()
        } else {
            f64::NAN
        };
        let trunc = if f.is_nan() { 0.0 } else { f.trunc() };
        if !trunc.is_finite() || !(2.0..=36.0).contains(&trunc) {
            return Err(IntrinsicError::OutOfRange {
                index: 0,
                reason: "radix must be an integer in [2, 36]",
            });
        }
        trunc as u32
    };
    let rendered = if radix == 10 {
        recv.to_decimal_string(args.gc_heap)
    } else {
        recv.with_inner(args.gc_heap, |b| b.to_str_radix(radix))
    };
    Ok(Value::string(JsString::from_str(&rendered, args.gc_heap)?))
}

/// §21.2.3.5 `BigInt.prototype.valueOf()` — returns the receiver.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-bigint.prototype.valueof>
fn impl_value_of(args: &mut IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::big_int(receiver_bigint(args)?))
}

/// Declarative `BigInt.prototype` table.
pub static BIGINT_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            BigInt,
            "toString" / 1 => impl_to_string,
            "valueOf"  / 0 => impl_value_of,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    BIGINT_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::BigInt, name)
}
