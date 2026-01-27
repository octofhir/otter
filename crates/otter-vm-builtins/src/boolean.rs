//! Boolean built-in
//!
//! Provides Boolean constructor and Boolean.prototype methods:
//! - valueOf - returns primitive boolean value
//! - toString - returns "true" or "false"

use otter_vm_core::memory;
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Boolean ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Boolean_valueOf", boolean_value_of),
        op_native("__Boolean_toString", boolean_to_string),
    ]
}

// =============================================================================
// Helper functions
// =============================================================================

/// Convert a value to boolean (ToBoolean abstract operation)
fn to_boolean(val: &Value) -> bool {
    if val.is_undefined() || val.is_null() {
        false
    } else if let Some(b) = val.as_boolean() {
        b
    } else if let Some(n) = val.as_number() {
        // false for +0, -0, NaN; true otherwise
        !n.is_nan() && n != 0.0
    } else if let Some(n) = val.as_int32() {
        n != 0
    } else if let Some(s) = val.as_string() {
        !s.as_str().is_empty()
    } else {
        // Objects are always truthy
        true
    }
}

// =============================================================================
// Prototype methods
// =============================================================================

/// Boolean.prototype.valueOf() - returns the primitive value of a Boolean object
fn boolean_value_of(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    match args.first() {
        Some(v) if v.is_boolean() => Ok(v.clone()),
        Some(v) => {
            // For non-boolean, coerce to boolean
            Ok(Value::boolean(to_boolean(v)))
        }
        None => Ok(Value::boolean(false)),
    }
}

/// Boolean.prototype.toString() - returns a string representing the boolean
fn boolean_to_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let b = match args.first() {
        Some(v) if v.is_boolean() => v.as_boolean().unwrap(),
        Some(v) => to_boolean(v),
        None => false,
    };

    Ok(Value::string(JsString::intern(if b {
        "true"
    } else {
        "false"
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn str_val(s: &str) -> Value {
        Value::string(JsString::intern(s))
    }

    fn assert_str_result(result: &Value, expected: &str) {
        let s = result.as_string().expect("expected string value");
        assert_eq!(s.as_str(), expected);
    }

    #[test]
    fn test_value_of_boolean() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        assert_eq!(
            boolean_value_of(&[Value::boolean(true)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            boolean_value_of(&[Value::boolean(false)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_value_of_coercion() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        // Numbers
        assert_eq!(
            boolean_value_of(&[Value::number(0.0)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            boolean_value_of(&[Value::number(42.0)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            boolean_value_of(&[Value::number(f64::NAN)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );

        // Integers
        assert_eq!(
            boolean_value_of(&[Value::int32(0)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            boolean_value_of(&[Value::int32(1)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );

        // Strings
        assert_eq!(
            boolean_value_of(&[str_val("")], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            boolean_value_of(&[str_val("hello")], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );

        // Null/undefined
        assert_eq!(
            boolean_value_of(&[Value::null()], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            boolean_value_of(&[Value::undefined()], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_to_string() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let result = boolean_to_string(&[Value::boolean(true)], memory_manager.clone()).unwrap();
        assert_str_result(&result, "true");

        let result = boolean_to_string(&[Value::boolean(false)], memory_manager.clone()).unwrap();
        assert_str_result(&result, "false");
    }

    #[test]
    fn test_to_string_coercion() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        // Truthy values
        let result = boolean_to_string(&[Value::number(42.0)], memory_manager.clone()).unwrap();
        assert_str_result(&result, "true");

        // Falsy values
        let result = boolean_to_string(&[Value::number(0.0)], memory_manager.clone()).unwrap();
        assert_str_result(&result, "false");

        let result = boolean_to_string(&[str_val("")], memory_manager.clone()).unwrap();
        assert_str_result(&result, "false");

        let result = boolean_to_string(&[Value::null()], memory_manager.clone()).unwrap();
        assert_str_result(&result, "false");
    }
}
