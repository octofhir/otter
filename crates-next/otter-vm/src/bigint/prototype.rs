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
    match args.receiver {
        Value::BigInt(b) => Ok(b.clone()),
        _ => Err(IntrinsicError::BadReceiver { expected: "bigint" }),
    }
}

/// §21.2.3.4 `BigInt.prototype.toString(radix = 10)`.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-bigint.prototype.tostring>
fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let recv = receiver_bigint(args)?;
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
        recv.to_decimal_string()
    } else {
        recv.as_inner().to_str_radix(radix)
    };
    Ok(Value::String(JsString::from_str(
        &rendered,
        args.string_heap,
    )?))
}

/// §21.2.3.5 `BigInt.prototype.valueOf()` — returns the receiver.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-bigint.prototype.valueof>
fn impl_value_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::BigInt(receiver_bigint(args)?))
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
