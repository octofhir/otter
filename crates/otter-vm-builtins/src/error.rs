//! Error built-in
//!
//! Provides Error constructor and Error subclasses:
//! - Error - base error class
//! - TypeError - type errors
//! - ReferenceError - reference errors
//! - SyntaxError - syntax errors
//! - RangeError - range errors
//! - URIError - URI errors
//! - EvalError - eval errors

use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};
use std::sync::Arc;

/// Get Error ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Error_create", error_create),
        op_native("__Error_getMessage", error_get_message),
        op_native("__Error_getName", error_get_name),
        op_native("__Error_getStack", error_get_stack),
        op_native("__Error_setStack", error_set_stack),
        op_native("__Error_toString", error_to_string),
    ]
}

// =============================================================================
// Error creation and methods
// =============================================================================

/// Create an error object with name, message, and optional stack
/// Args: [name: string, message: string | undefined, stack: string | undefined]
fn error_create(args: &[Value]) -> Result<Value, String> {
    let name = args
        .first()
        .and_then(|v| v.as_string())
        .map(|s| s.as_str().to_string())
        .unwrap_or_else(|| "Error".to_string());

    let message = args
        .get(1)
        .and_then(|v| {
            if v.is_undefined() {
                None
            } else {
                v.as_string().map(|s| s.as_str().to_string())
            }
        })
        .unwrap_or_default();

    let stack = args.get(2).and_then(|v| {
        if v.is_undefined() {
            None
        } else {
            v.as_string().map(|s| s.as_str().to_string())
        }
    });

    // Create error object
    let obj = Arc::new(JsObject::new(None));

    // Set name property
    obj.set(
        PropertyKey::string("name"),
        Value::string(JsString::intern(&name)),
    );

    // Set message property
    obj.set(
        PropertyKey::string("message"),
        Value::string(JsString::intern(&message)),
    );

    // Set stack property (includes name: message at the top)
    let stack_str = if let Some(trace) = stack {
        if message.is_empty() {
            format!("{}\n{}", name, trace)
        } else {
            format!("{}: {}\n{}", name, message, trace)
        }
    } else if message.is_empty() {
        name.clone()
    } else {
        format!("{}: {}", name, message)
    };
    obj.set(
        PropertyKey::string("stack"),
        Value::string(JsString::intern(&stack_str)),
    );

    // Mark as error object (for instanceof checks)
    obj.set(PropertyKey::string("__isError__"), Value::boolean(true));

    // Store the error type for instanceof checks
    obj.set(
        PropertyKey::string("__errorType__"),
        Value::string(JsString::intern(&name)),
    );

    Ok(Value::object(obj))
}

/// Get error message
fn error_get_message(args: &[Value]) -> Result<Value, String> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or("Error.message requires an error object")?;

    let message = obj
        .get(&PropertyKey::string("message"))
        .unwrap_or_else(Value::undefined);

    Ok(message)
}

/// Get error name
fn error_get_name(args: &[Value]) -> Result<Value, String> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or("Error.name requires an error object")?;

    let name = obj
        .get(&PropertyKey::string("name"))
        .unwrap_or_else(|| Value::string(JsString::intern("Error")));

    Ok(name)
}

/// Get error stack trace
fn error_get_stack(args: &[Value]) -> Result<Value, String> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or("Error.stack requires an error object")?;

    let stack = obj
        .get(&PropertyKey::string("stack"))
        .unwrap_or_else(Value::undefined);

    Ok(stack)
}

/// Set error stack trace (used by Error.captureStackTrace)
fn error_set_stack(args: &[Value]) -> Result<Value, String> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or("Error.setStack requires an error object")?;

    let stack = args.get(1).cloned().unwrap_or_else(Value::undefined);

    obj.set(PropertyKey::string("stack"), stack);

    Ok(Value::undefined())
}

/// Error.prototype.toString() - returns "name: message" or just "name"
fn error_to_string(args: &[Value]) -> Result<Value, String> {
    let obj = args
        .first()
        .and_then(|v| v.as_object())
        .ok_or("Error.toString requires an error object")?;

    let name = obj
        .get(&PropertyKey::string("name"))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_else(|| "Error".to_string());

    let message = obj
        .get(&PropertyKey::string("message"))
        .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
        .unwrap_or_default();

    let result = if message.is_empty() {
        name
    } else {
        format!("{}: {}", name, message)
    };

    Ok(Value::string(JsString::intern(&result)))
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
    fn test_error_create_basic() {
        let result = error_create(&[
            str_val("Error"),
            str_val("something went wrong"),
            Value::undefined(),
        ])
        .unwrap();

        assert!(result.is_object());
        let obj = result.as_object().unwrap();

        // Check name
        let name = obj.get(&PropertyKey::string("name")).unwrap();
        assert_str_result(&name, "Error");

        // Check message
        let message = obj.get(&PropertyKey::string("message")).unwrap();
        assert_str_result(&message, "something went wrong");

        // Check stack (should contain "Error: something went wrong")
        let stack = obj.get(&PropertyKey::string("stack")).unwrap();
        let stack_str = stack.as_string().unwrap().as_str();
        assert!(stack_str.contains("Error: something went wrong"));
    }

    #[test]
    fn test_error_create_type_error() {
        let result = error_create(&[
            str_val("TypeError"),
            str_val("not a function"),
            Value::undefined(),
        ])
        .unwrap();

        let obj = result.as_object().unwrap();

        let name = obj.get(&PropertyKey::string("name")).unwrap();
        assert_str_result(&name, "TypeError");

        let message = obj.get(&PropertyKey::string("message")).unwrap();
        assert_str_result(&message, "not a function");
    }

    #[test]
    fn test_error_create_no_message() {
        let result =
            error_create(&[str_val("Error"), Value::undefined(), Value::undefined()]).unwrap();

        let obj = result.as_object().unwrap();

        let message = obj.get(&PropertyKey::string("message")).unwrap();
        assert_str_result(&message, "");

        let stack = obj.get(&PropertyKey::string("stack")).unwrap();
        assert_str_result(&stack, "Error");
    }

    #[test]
    fn test_error_create_with_stack() {
        let stack_trace = "    at foo (test.js:1:1)\n    at bar (test.js:2:2)";
        let result = error_create(&[
            str_val("Error"),
            str_val("test error"),
            str_val(stack_trace),
        ])
        .unwrap();

        let obj = result.as_object().unwrap();
        let stack = obj.get(&PropertyKey::string("stack")).unwrap();
        let stack_str = stack.as_string().unwrap().as_str();

        assert!(stack_str.starts_with("Error: test error"));
        assert!(stack_str.contains("at foo"));
        assert!(stack_str.contains("at bar"));
    }

    #[test]
    fn test_error_get_message() {
        let err = error_create(&[str_val("Error"), str_val("hello"), Value::undefined()]).unwrap();
        let result = error_get_message(std::slice::from_ref(&err)).unwrap();
        assert_str_result(&result, "hello");
    }

    #[test]
    fn test_error_get_name() {
        let err =
            error_create(&[str_val("TypeError"), str_val("oops"), Value::undefined()]).unwrap();
        let result = error_get_name(std::slice::from_ref(&err)).unwrap();
        assert_str_result(&result, "TypeError");
    }

    #[test]
    fn test_error_to_string() {
        let err = error_create(&[
            str_val("RangeError"),
            str_val("out of bounds"),
            Value::undefined(),
        ])
        .unwrap();
        let result = error_to_string(std::slice::from_ref(&err)).unwrap();
        assert_str_result(&result, "RangeError: out of bounds");
    }

    #[test]
    fn test_error_to_string_no_message() {
        let err =
            error_create(&[str_val("Error"), Value::undefined(), Value::undefined()]).unwrap();
        let result = error_to_string(std::slice::from_ref(&err)).unwrap();
        assert_str_result(&result, "Error");
    }

    #[test]
    fn test_error_set_stack() {
        let err = error_create(&[str_val("Error"), str_val("test"), Value::undefined()]).unwrap();
        let new_stack = str_val("new stack trace");

        error_set_stack(&[err.clone(), new_stack]).unwrap();

        let stack = error_get_stack(std::slice::from_ref(&err)).unwrap();
        assert_str_result(&stack, "new stack trace");
    }
}
