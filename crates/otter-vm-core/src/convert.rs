//! Value conversion traits for native function parameter marshalling.
//!
//! `FromValue` converts a JS `Value` into a Rust type (for function parameters).
//! `IntoValue` converts a Rust type back into a JS `Value` (for return values).
//!
//! These traits are used by the `#[dive]` macro to auto-generate conversion code.

use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::JsObject;
use crate::string::JsString;
use crate::value::Value;

/// Convert a JS `Value` into a Rust type.
///
/// Used by `#[dive]` macro for automatic parameter conversion.
pub trait FromValue: Sized {
    /// Convert from a JS Value, returning VmError on type mismatch.
    fn from_value(value: &Value) -> Result<Self, VmError>;
}

/// Convert a Rust type into a JS `Value`.
///
/// Used by `#[dive]` macro for automatic return value conversion.
pub trait IntoValue {
    /// Convert into a JS Value.
    fn into_value(self) -> Value;
}

// ---------------------------------------------------------------------------
// FromValue implementations
// ---------------------------------------------------------------------------

impl FromValue for f64 {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        if let Some(n) = value.as_number() {
            Ok(n)
        } else if let Some(n) = value.as_int32() {
            Ok(n as f64)
        } else if value.is_undefined() {
            Ok(f64::NAN)
        } else if value.is_null() {
            Ok(0.0)
        } else if value.is_boolean() {
            Ok(if value.to_boolean() { 1.0 } else { 0.0 })
        } else if let Some(s) = value.as_string() {
            let trimmed = s.as_str().trim();
            if trimmed.is_empty() {
                Ok(0.0)
            } else {
                trimmed.parse::<f64>().unwrap_or(f64::NAN).pipe(Ok)
            }
        } else {
            Ok(f64::NAN)
        }
    }
}

impl FromValue for i32 {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        if let Some(n) = value.as_int32() {
            Ok(n)
        } else {
            let n = f64::from_value(value)?;
            Ok(to_int32(n))
        }
    }
}

impl FromValue for u32 {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        if let Some(n) = value.as_int32() {
            Ok(n as u32)
        } else {
            let n = f64::from_value(value)?;
            Ok(to_uint32(n))
        }
    }
}

impl FromValue for bool {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        Ok(value.to_boolean())
    }
}

impl FromValue for String {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        if let Some(s) = value.as_string() {
            Ok(s.as_str().to_string())
        } else if value.is_undefined() {
            Ok("undefined".to_string())
        } else if value.is_null() {
            Ok("null".to_string())
        } else if value.is_boolean() {
            Ok(if value.to_boolean() {
                "true".to_string()
            } else {
                "false".to_string()
            })
        } else if let Some(n) = value.as_int32() {
            Ok(n.to_string())
        } else if let Some(n) = value.as_number() {
            Ok(format_number(n))
        } else {
            // Objects, functions, etc. — ToString is complex, fallback
            Ok("[object Object]".to_string())
        }
    }
}

impl FromValue for Value {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        Ok(value.clone())
    }
}

impl FromValue for GcRef<JsObject> {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        value
            .as_object()
            .ok_or_else(|| VmError::type_error("Expected an object"))
    }
}

impl<T: FromValue> FromValue for Option<T> {
    fn from_value(value: &Value) -> Result<Self, VmError> {
        if value.is_undefined() || value.is_null() {
            Ok(None)
        } else {
            T::from_value(value).map(Some)
        }
    }
}

// ---------------------------------------------------------------------------
// IntoValue implementations
// ---------------------------------------------------------------------------

impl IntoValue for Value {
    fn into_value(self) -> Value {
        self
    }
}

impl IntoValue for f64 {
    fn into_value(self) -> Value {
        Value::number(self)
    }
}

impl IntoValue for i32 {
    fn into_value(self) -> Value {
        Value::int32(self)
    }
}

impl IntoValue for u32 {
    fn into_value(self) -> Value {
        Value::number(self as f64)
    }
}

impl IntoValue for bool {
    fn into_value(self) -> Value {
        Value::boolean(self)
    }
}

impl IntoValue for String {
    fn into_value(self) -> Value {
        Value::string(JsString::new_gc(&self))
    }
}

impl IntoValue for &str {
    fn into_value(self) -> Value {
        Value::string(JsString::new_gc(self))
    }
}

impl IntoValue for () {
    fn into_value(self) -> Value {
        Value::undefined()
    }
}

impl<T: IntoValue> IntoValue for Option<T> {
    fn into_value(self) -> Value {
        match self {
            Some(v) => v.into_value(),
            None => Value::undefined(),
        }
    }
}

impl IntoValue for GcRef<JsObject> {
    fn into_value(self) -> Value {
        Value::object(self)
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// ES2023 ToInt32 abstract operation.
fn to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = n.trunc() as i64;
    (i % (1_i64 << 32)) as i32
}

/// ES2023 ToUint32 abstract operation.
fn to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = n.trunc() as i64;
    (i % (1_i64 << 32)) as u32
}

/// Format a number to string per JS semantics.
fn format_number(n: f64) -> String {
    crate::globals::js_number_to_string(n)
}

/// Helper trait for pipe operations (used internally).
trait Pipe: Sized {
    fn pipe<R>(self, f: impl FnOnce(Self) -> R) -> R {
        f(self)
    }
}

impl<T> Pipe for T {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_f64_from_value() {
        assert_eq!(f64::from_value(&Value::number(42.5)).unwrap(), 42.5);
        assert_eq!(f64::from_value(&Value::int32(10)).unwrap(), 10.0);
        assert!(f64::from_value(&Value::undefined()).unwrap().is_nan());
        assert_eq!(f64::from_value(&Value::null()).unwrap(), 0.0);
        assert_eq!(f64::from_value(&Value::boolean(true)).unwrap(), 1.0);
        assert_eq!(f64::from_value(&Value::boolean(false)).unwrap(), 0.0);
    }

    #[test]
    fn test_i32_from_value() {
        assert_eq!(i32::from_value(&Value::int32(42)).unwrap(), 42);
        assert_eq!(i32::from_value(&Value::number(3.14)).unwrap(), 3);
        assert_eq!(i32::from_value(&Value::undefined()).unwrap(), 0);
    }

    #[test]
    fn test_bool_from_value() {
        assert!(bool::from_value(&Value::boolean(true)).unwrap());
        assert!(!bool::from_value(&Value::boolean(false)).unwrap());
        assert!(!bool::from_value(&Value::undefined()).unwrap());
        assert!(!bool::from_value(&Value::null()).unwrap());
        assert!(!bool::from_value(&Value::int32(0)).unwrap());
        assert!(bool::from_value(&Value::int32(1)).unwrap());
    }

    #[test]
    fn test_option_from_value() {
        assert_eq!(
            Option::<f64>::from_value(&Value::undefined()).unwrap(),
            None
        );
        assert_eq!(Option::<f64>::from_value(&Value::null()).unwrap(), None);
        assert_eq!(
            Option::<f64>::from_value(&Value::number(42.0)).unwrap(),
            Some(42.0)
        );
    }

    #[test]
    fn test_into_value() {
        assert_eq!(42.0_f64.into_value().as_number(), Some(42.0));
        assert_eq!(42_i32.into_value().as_int32(), Some(42));
        assert_eq!(true.into_value().to_boolean(), true);
        assert!(().into_value().is_undefined());
    }

    #[test]
    fn test_to_int32() {
        assert_eq!(to_int32(f64::NAN), 0);
        assert_eq!(to_int32(f64::INFINITY), 0);
        assert_eq!(to_int32(0.0), 0);
        assert_eq!(to_int32(3.14), 3);
        assert_eq!(to_int32(-3.14), -3);
    }

    #[test]
    fn test_to_uint32() {
        assert_eq!(to_uint32(f64::NAN), 0);
        assert_eq!(to_uint32(3.14), 3);
    }
}
