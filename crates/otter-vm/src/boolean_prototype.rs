//! `Boolean.prototype.*` intrinsic implementations.
//!
//! Foundation Booleans are primitives — `Boolean(x)` returns a
//! plain `Value::Boolean`, not a wrapper Object — so prototype
//! methods receive a `Value::Boolean` receiver directly.
//!
//! # Contents
//! - [`BOOLEAN_PROTOTYPE_TABLE`] — declarative table built with
//!   the [`crate::intrinsics!`] macro.
//! - [`lookup`] — convenience accessor used by the dispatcher.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-properties-of-the-boolean-prototype-object>
use crate::Value;
use crate::intrinsics::{IntrinsicArgs, IntrinsicError, IntrinsicReceiver, IntrinsicTable};
use crate::string::JsString;

fn receiver_bool(args: &IntrinsicArgs<'_>) -> Result<bool, IntrinsicError> {
    match args.receiver {
        Value::Boolean(b) => Ok(*b),
        _ => Err(IntrinsicError::BadReceiver {
            expected: "boolean",
        }),
    }
}

/// §20.3.3.2 Boolean.prototype.toString — `"true"` / `"false"`.
fn impl_to_string(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    let s = if receiver_bool(args)? {
        "true"
    } else {
        "false"
    };
    Ok(Value::String(JsString::from_str(s, args.string_heap)?))
}

/// §20.3.3.3 Boolean.prototype.valueOf — returns the receiver.
fn impl_value_of(args: &IntrinsicArgs<'_>) -> Result<Value, IntrinsicError> {
    Ok(Value::Boolean(receiver_bool(args)?))
}

/// Declarative `Boolean.prototype` table.
pub static BOOLEAN_PROTOTYPE_TABLE: std::sync::LazyLock<IntrinsicTable> =
    std::sync::LazyLock::new(|| {
        crate::intrinsics!(
            Boolean,
            "toString" / 0 => impl_to_string,
            "valueOf"  / 0 => impl_value_of,
        )
    });

/// Convenience accessor used by the dispatcher.
#[must_use]
pub fn lookup(name: &str) -> Option<&'static crate::intrinsics::IntrinsicEntry> {
    BOOLEAN_PROTOTYPE_TABLE.lookup(IntrinsicReceiver::Boolean, name)
}
