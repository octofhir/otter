//! Binary-data values: `ArrayBuffer`, `DataView`, and the eleven
//! `TypedArray` constructors.
//!
//! Foundation surface implements ECMA-262 §25.1 / §25.3 / §23.2 in
//! full: every constructor overload, every prototype method, and
//! every static method named in those sections lives here. Crypto /
//! `WebAssembly.Memory` integration is out of scope for the runtime
//! slice; this module is the language-level surface.
//!
//! # Layout
//! - [`array_buffer`] — `JsArrayBuffer` value type.
//! - [`data_view`] — `JsDataView` value type.
//! - [`typed_array`] — `JsTypedArray` value type and
//!   [`typed_array::TypedArrayKind`] enum.
//! - [`dispatch`] — opcode dispatchers for `Op::ArrayBufferCall`,
//!   `Op::DataViewCall`, `Op::TypedArrayCall`.
//! - [`array_buffer_prototype`] / [`data_view_prototype`] /
//!   [`typed_array_prototype`] — prototype-method intrinsic tables.
//!
//! # See also
//! - <https://tc39.es/ecma262/#sec-arraybuffer-objects>
//! - <https://tc39.es/ecma262/#sec-typedarray-objects>
//! - <https://tc39.es/ecma262/#sec-dataview-objects>

pub mod array_buffer;
pub mod array_buffer_prototype;
pub mod data_view;
pub mod data_view_prototype;
pub mod dispatch;
pub mod typed_array;
pub mod typed_array_prototype;

pub use array_buffer::JsArrayBuffer;
pub use data_view::JsDataView;
pub use typed_array::{JsTypedArray, TypedArrayKind};

use crate::Value;
use crate::number::NumberValue;

/// §7.1.22 `ToIndex(value)` — coerce to a non-negative integer in
/// `[0, 2^53 - 1]`. Returns `None` for non-finite, negative, or
/// non-integer inputs (the dispatcher surfaces a
/// [`crate::VmError::TypeMismatch`]).
#[must_use]
pub fn to_index(value: &Value) -> Option<u64> {
    // §7.1.17 ToIndex(value):
    // 1. If value is undefined → return 0.
    // 2. Else let integer = ToIntegerOrInfinity(value).
    // 3. If integer < 0 or integer > 2^53-1 → RangeError (return None).
    // §7.1.5 ToIntegerOrInfinity: ToNumber(value); NaN → 0;
    // +∞ / -∞ pass through; else truncate toward 0.
    let n = match value {
        Value::Undefined => return Some(0),
        Value::Number(n) => n.as_f64(),
        Value::Boolean(true) => 1.0,
        Value::Boolean(false) | Value::Null => 0.0,
        Value::String(s) => crate::number::to_number_from_string(&s.to_lossy_string()).as_f64(),
        _ => return None,
    };
    if n.is_nan() {
        return Some(0);
    }
    if !n.is_finite() || n < 0.0 || n > 9_007_199_254_740_991.0 {
        return None;
    }
    Some(n.trunc() as u64)
}

/// §7.1.4 `ToNumber` for the `littleEndian` flag — a missing or
/// `undefined` argument falls back to `false` (big-endian, the spec
/// default). Truthiness on the value follows §7.1.2 `ToBoolean`.
#[must_use]
pub fn to_little_endian_flag(value: Option<&Value>) -> bool {
    match value {
        None | Some(Value::Undefined) => false,
        Some(v) => v.to_boolean(),
    }
}

/// Construct a `Value::Number` from an `f64`.
#[must_use]
pub fn number_value(n: f64) -> Value {
    Value::Number(NumberValue::from_f64(n))
}

/// Construct a `Value::Number` from a small integer.
#[must_use]
pub fn smi(n: i32) -> Value {
    Value::Number(NumberValue::from_i32(n))
}

/// Spec-canonical `[Symbol.toStringTag]` rendering for binary
/// values. Used by the `Object.prototype.toString` intercept and
/// by display formatting.
///
/// # See also
/// - <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype-%40%40tostringtag>
/// - <https://tc39.es/ecma262/#sec-get-%25typedarray%25.prototype-%40%40tostringtag>
/// - <https://tc39.es/ecma262/#sec-get-dataview.prototype-%40%40tostringtag>
#[must_use]
pub fn to_string_tag_for(value: &Value) -> Option<&'static str> {
    Some(match value {
        Value::ArrayBuffer(_) => "ArrayBuffer",
        Value::DataView(_) => "DataView",
        Value::TypedArray(t) => t.kind().name(),
        _ => return None,
    })
}
