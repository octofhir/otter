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

pub use array_buffer::{
    BufferStorage, JsArrayBuffer, LOCAL_ARRAY_BUFFER_BODY_TYPE_TAG, LocalArrayBufferBodyGc,
    LocalArrayBufferHandle, SHARED_ARRAY_BUFFER_BODY_TYPE_TAG, SharedArrayBufferBodyGc,
    SharedArrayBufferHandle, alloc_local_array_buffer, alloc_shared_array_buffer,
};
pub use data_view::{
    DATA_VIEW_BODY_TYPE_TAG, DataViewBodyGc, DataViewHandle, JsDataView, alloc_data_view,
};
pub use typed_array::{
    JsTypedArray, TYPED_ARRAY_BODY_TYPE_TAG, TypedArrayBodyGc, TypedArrayHandle, TypedArrayKind,
    alloc_typed_array,
};

use crate::Value;

/// §7.1.22 `ToIndex(value)` — coerce to a non-negative integer in
/// `[0, 2^53 - 1]`. Returns `None` for non-integer inputs after
/// `ToIntegerOrInfinity` (`±∞`, BigInt / Symbol, or out-of-range)
/// so the dispatcher can surface a [`crate::VmError::TypeMismatch`]
/// / `RangeError`. The truncation step in
/// [`ToIntegerOrInfinity`](https://tc39.es/ecma262/#sec-tointegerorinfinity)
/// runs *before* the negative / range check, so callers like
/// `sample.getUint8(-0.9)` and `sample.getBigInt64("-0.4")` resolve
/// to index `0` rather than throwing.
///
/// <https://tc39.es/ecma262/#sec-toindex>
#[must_use]
pub fn to_index(value: &Value, heap: &otter_gc::GcHeap) -> Option<u64> {
    if value.is_undefined() {
        return Some(0);
    }
    let n = if let Some(n) = value.as_number() {
        n.as_f64()
    } else if let Some(b) = value.as_boolean() {
        if b { 1.0 } else { 0.0 }
    } else if value.is_null() {
        0.0
    } else if let Some(s) = value.as_string() {
        crate::number::to_number_from_string(&s.to_lossy_string(heap)).as_f64()
    } else {
        return None;
    };
    if n.is_nan() {
        return Some(0);
    }
    if !n.is_finite() {
        return None;
    }
    // §7.1.5 ToIntegerOrInfinity truncates toward 0 *first*; the
    // §7.1.22 ToIndex range check then runs on the truncated value
    // so `-0.9` (which truncates to `-0`) is a valid index, not a
    // `RangeError`.
    let truncated = n.trunc();
    if !(0.0..=9_007_199_254_740_991.0).contains(&truncated) {
        return None;
    }
    Some(truncated as u64)
}

/// §7.1.4 `ToNumber` for the `littleEndian` flag — a missing or
/// `undefined` argument falls back to `false` (big-endian, the spec
/// default). Truthiness on the value follows §7.1.2 `ToBoolean`.
#[must_use]
pub fn to_little_endian_flag(value: Option<&Value>, heap: &otter_gc::GcHeap) -> bool {
    match value {
        None => false,
        Some(v) if v.is_undefined() => false,
        Some(v) => v.to_boolean(heap),
    }
}

/// Construct a `Value::Number` from an `f64`.
#[must_use]
pub fn number_value(n: f64) -> Value {
    Value::number_f64(n)
}

/// Construct a `Value::Number` from a small integer.
#[must_use]
pub fn smi(n: i32) -> Value {
    Value::number_i32(n)
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
    if value.is_array_buffer() {
        return Some("ArrayBuffer");
    }
    if value.is_data_view() {
        return Some("DataView");
    }
    if let Some(t) = value.as_typed_array() {
        return Some(t.kind().name());
    }
    None
}
