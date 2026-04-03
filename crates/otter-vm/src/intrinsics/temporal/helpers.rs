//! Shared helpers for Temporal intrinsics.
//!
//! Provides error mapping, option parsing, and common spec operations
//! used across all Temporal types.
//!
//! Spec: <https://tc39.es/proposal-temporal/>

use crate::descriptors::VmNativeCallError;
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

/// Returns a reference to the compiled IANA timezone provider.
///
/// Requires the `compiled_data` feature on `temporal_rs`.
#[allow(dead_code)]
pub fn tz_provider() -> &'static impl temporal_rs::provider::TimeZoneProvider {
    &*temporal_rs::provider::COMPILED_TZ_PROVIDER
}

// ── Error mapping ───────────────────────────────────────────────────

/// Maps a `temporal_rs::TemporalError` to a `VmNativeCallError`.
///
/// §2.4 Temporal error handling: TemporalError::Type → TypeError,
/// TemporalError::Range → RangeError.
pub fn temporal_err(
    e: temporal_rs::error::TemporalError,
    runtime: &mut crate::interpreter::RuntimeState,
) -> VmNativeCallError {
    let raw = format!("{e}");
    let msg = raw
        .strip_prefix("TypeError: ")
        .or_else(|| raw.strip_prefix("RangeError: "))
        .or_else(|| raw.strip_prefix("SyntaxError: "))
        .unwrap_or(&raw);
    match e.kind() {
        temporal_rs::error::ErrorKind::Type => type_error(runtime, msg),
        temporal_rs::error::ErrorKind::Range | temporal_rs::error::ErrorKind::Syntax => {
            range_error(runtime, msg)
        }
        _ => type_error(runtime, msg),
    }
}

/// Allocates a TypeError and returns it as a throwable error.
pub fn type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    msg: &str,
) -> VmNativeCallError {
    match runtime.alloc_type_error(msg) {
        Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
        Err(error) => VmNativeCallError::Internal(format!("TypeError alloc failed: {error}").into()),
    }
}

/// Allocates a RangeError and returns it as a throwable error.
pub fn range_error(
    runtime: &mut crate::interpreter::RuntimeState,
    msg: &str,
) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg_str = runtime.alloc_string(msg);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg_str.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

// ── Argument extraction ─────────────────────────────────────────────

/// Extracts an argument as `f64`, defaulting to 0 if missing.
pub fn to_integer_or_zero(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    let val = args
        .get(index)
        .copied()
        .unwrap_or(RegisterValue::undefined());
    if val == RegisterValue::undefined() {
        return Ok(0.0);
    }
    runtime
        .js_to_number(val)
        .map(|n| if n.is_nan() { 0.0 } else { n.trunc() })
        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))
}

/// Extracts an argument as a Rust `String`.
pub fn to_string_arg(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<String, VmNativeCallError> {
    let val = args
        .get(index)
        .copied()
        .unwrap_or(RegisterValue::undefined());
    runtime
        .js_to_string(val)
        .map(|s| s.into_string())
        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))
}

/// Extracts an argument as BigInt i128 (for epoch nanoseconds).
pub fn to_bigint_i128(
    args: &[RegisterValue],
    index: usize,
    runtime: &crate::interpreter::RuntimeState,
) -> Result<i128, VmNativeCallError> {
    let val = args
        .get(index)
        .copied()
        .unwrap_or(RegisterValue::undefined());
    let handle = val
        .as_bigint_handle()
        .ok_or_else(|| {
            VmNativeCallError::Internal(
                "Temporal.Instant requires a BigInt epoch nanoseconds argument".into(),
            )
        })?;
    let s = runtime
        .bigint_value(ObjectHandle(handle))
        .ok_or_else(|| VmNativeCallError::Internal("invalid BigInt handle".into()))?;
    s.parse::<i128>()
        .map_err(|_| VmNativeCallError::Internal("BigInt value too large for i128".into()))
}

// ── Temporal.*.prototype.valueOf ─────────────────────────────────────

/// §2.3.46 All Temporal types throw TypeError from valueOf.
/// <https://tc39.es/proposal-temporal/#sec-temporal.instant.prototype.valueof>
pub fn temporal_value_of(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Err(type_error(
        runtime,
        "Temporal objects do not support valueOf. Use equals() or compare() instead.",
    ))
}
