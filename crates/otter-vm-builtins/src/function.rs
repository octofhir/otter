//! Function built-in
//!
//! Provides Function constructor and Function.prototype methods:
//! - call - calls function with given this value and arguments
//! - apply - calls function with given this value and arguments array
//! - bind - creates bound function with fixed this and partial arguments
//! - toString - returns string representation of function
//! - name - returns function name
//! - length - returns function parameter count

use otter_vm_core::gc::GcRef;
use otter_vm_core::memory;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use std::sync::Arc;

/// Get Function ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__Function_call", function_call),
        op_native("__Function_apply", function_apply),
        op_native("__Function_toString", function_to_string),
        op_native("__Function_getName", function_get_name),
        op_native("__Function_getLength", function_get_length),
        op_native("__Function_isFunction", function_is_function),
        op_native("__Function_createBound", function_create_bound),
    ]
}

// =============================================================================
// Function methods
// =============================================================================

/// Function.prototype.call(thisArg, ...args)
/// Actual dispatch is handled in the interpreter for correct `this` binding.
fn function_call(_args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    Err("Function.call is internal".to_string())
}

/// Function.prototype.apply(thisArg, argsArray)
/// Actual dispatch is handled in the interpreter for correct `this` binding.
fn function_apply(_args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    Err("Function.apply is internal".to_string())
}

/// Function.prototype.toString() - returns string representation
fn function_to_string(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let func = args
        .first()
        .ok_or("Function.toString requires a function")?;

    let result = if func.is_function() {
        // For closures, we can return a generic representation
        // In a full implementation, we'd store and return the original source
        if let Some(closure) = func.as_function() {
            format!(
                "function {}() {{ [native code] }}",
                if closure.is_async { "async " } else { "" }
            )
        } else {
            "function() { [native code] }".to_string()
        }
    } else if func.is_native_function() {
        "function() { [native code] }".to_string()
    } else if let Some(obj) = func.as_object() {
        // Check if it's a bound function
        if obj.get(&PropertyKey::string("__boundFunction__")).is_some() {
            "function() { [bound] }".to_string()
        } else {
            return Err("Function.toString requires a function".to_string());
        }
    } else {
        return Err("Function.toString requires a function".to_string());
    };

    Ok(Value::string(JsString::intern(&result)))
}

/// Get function name
fn function_get_name(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let func = args.first().ok_or("Function.name requires a function")?;

    // Check for bound function first
    if let Some(obj) = func.as_object()
        && let Some(name) = obj.get(&PropertyKey::string("__boundName__"))
    {
        return Ok(name);
    }

    // For closures and native functions, return empty string for now
    // In full implementation, we'd get name from bytecode metadata
    if func.is_function() || func.is_native_function() {
        Ok(Value::string(JsString::intern("")))
    } else {
        Err("Function.name requires a function".to_string())
    }
}

/// Get function parameter count (length)
fn function_get_length(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let func = args.first().ok_or("Function.length requires a function")?;

    // Check for bound function first
    if let Some(obj) = func.as_object()
        && let Some(length) = obj.get(&PropertyKey::string("__boundLength__"))
    {
        return Ok(length);
    }

    // For closures, we could get param_count from bytecode
    // For now, return 0 as default
    if func.is_function() || func.is_native_function() {
        Ok(Value::int32(0))
    } else {
        Err("Function.length requires a function".to_string())
    }
}

/// Check if value is a function
fn function_is_function(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let val = args.first().ok_or("isFunction requires an argument")?;

    let is_func = val.is_callable()
        || val
            .as_object()
            .map(|o| o.get(&PropertyKey::string("__boundFunction__")).is_some())
            .unwrap_or(false);

    Ok(Value::boolean(is_func))
}

/// Create a bound function object
/// Args: [originalFunc, thisArg, ...boundArgs]
fn function_create_bound(args: &[Value], mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let original = args.first().ok_or("bind requires a function")?.clone();

    let this_arg = args.get(1).cloned().unwrap_or_else(Value::undefined);

    // Create bound function as an object with special properties
    let bound = GcRef::new(JsObject::new(None, mm.clone()));

    // Store the original function
    bound.set(PropertyKey::string("__boundFunction__"), original.clone());

    // Store the thisArg
    bound.set(PropertyKey::string("__boundThis__"), this_arg);

    // Store bound arguments (if any)
    if args.len() > 2 {
        let bound_args: Vec<Value> = args[2..].to_vec();
        // Store as array
        let arr = GcRef::new(JsObject::new(None, mm.clone()));
        for (i, arg) in bound_args.iter().enumerate() {
            arr.set(PropertyKey::Index(i as u32), arg.clone());
        }
        arr.set(
            PropertyKey::string("length"),
            Value::int32(bound_args.len() as i32),
        );
        bound.set(PropertyKey::string("__boundArgs__"), Value::object(arr));
    }

    // Set name property (bound <originalName>)
    let original_name: String = if original.is_function() || original.is_native_function() {
        String::new()
    } else if let Some(obj) = original.as_object() {
        if let Some(name) = obj.get(&PropertyKey::string("__boundName__")) {
            if let Some(s) = name.as_string() {
                // Already bound, extract base name
                let name_str = s.as_str();
                if name_str.starts_with("bound ") {
                    name_str.to_string()
                } else {
                    String::new()
                }
            } else {
                String::new()
            }
        } else {
            String::new()
        }
    } else {
        String::new()
    };
    let bound_name = format!("bound {}", original_name);
    bound.set(
        PropertyKey::string("__boundName__"),
        Value::string(JsString::intern(&bound_name)),
    );

    // Set length (original length - bound args count, min 0)
    let bound_args_len = if args.len() > 2 { args.len() - 2 } else { 0 };
    let new_length = 0i32.saturating_sub(bound_args_len as i32).max(0);
    bound.set(
        PropertyKey::string("__boundLength__"),
        Value::int32(new_length),
    );

    // Mark as callable for type checking
    bound.set(PropertyKey::string("__isCallable__"), Value::boolean(true));

    Ok(Value::object(bound))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_function_is_function() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        // Native function should be detected
        let native_fn =
            Value::native_function(|_args, _mm| Ok(Value::undefined()), memory_manager.clone());
        let result =
            function_is_function(std::slice::from_ref(&native_fn), memory_manager.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(true));

        // Non-function should return false
        let num = Value::int32(42);
        let result =
            function_is_function(std::slice::from_ref(&num), memory_manager.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }

    #[test]
    fn test_function_to_string_native() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let native_fn =
            Value::native_function(|_args, _mm| Ok(Value::undefined()), memory_manager.clone());
        let result =
            function_to_string(std::slice::from_ref(&native_fn), memory_manager.clone()).unwrap();
        let s = result.as_string().unwrap();
        assert!(s.as_str().contains("[native code]"));
    }

    #[test]
    fn test_function_get_name_default() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let native_fn =
            Value::native_function(|_args, _mm| Ok(Value::undefined()), memory_manager.clone());
        let result =
            function_get_name(std::slice::from_ref(&native_fn), memory_manager.clone()).unwrap();
        let s = result.as_string().unwrap();
        assert_eq!(s.as_str(), "");
    }

    #[test]
    fn test_function_get_length_default() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let native_fn =
            Value::native_function(|_args, _mm| Ok(Value::undefined()), memory_manager.clone());
        let result =
            function_get_length(std::slice::from_ref(&native_fn), memory_manager.clone()).unwrap();
        assert_eq!(result.as_int32(), Some(0));
    }

    #[test]
    fn test_function_create_bound() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let original =
            Value::native_function(|_args, _mm| Ok(Value::int32(42)), memory_manager.clone());
        let this_arg = Value::undefined();

        let result = function_create_bound(&[original, this_arg], memory_manager.clone()).unwrap();

        // Should be an object
        assert!(result.is_object());
        let obj = result.as_object().unwrap();

        // Should have __boundFunction__
        assert!(obj.get(&PropertyKey::string("__boundFunction__")).is_some());

        // Should have __boundThis__
        assert!(obj.get(&PropertyKey::string("__boundThis__")).is_some());
    }

    #[test]
    fn test_function_create_bound_with_args() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let original =
            Value::native_function(|_args, _mm| Ok(Value::int32(42)), memory_manager.clone());
        let this_arg = Value::undefined();
        let arg1 = Value::int32(1);
        let arg2 = Value::int32(2);

        let result =
            function_create_bound(&[original, this_arg, arg1, arg2], memory_manager.clone())
                .unwrap();

        let obj = result.as_object().unwrap();

        // Should have __boundArgs__
        let bound_args = obj.get(&PropertyKey::string("__boundArgs__"));
        assert!(bound_args.is_some());
    }

    #[test]
    fn test_bound_function_is_callable() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let original =
            Value::native_function(|_args, _mm| Ok(Value::int32(42)), memory_manager.clone());
        let bound =
            function_create_bound(&[original, Value::undefined()], memory_manager.clone()).unwrap();

        let result =
            function_is_function(std::slice::from_ref(&bound), memory_manager.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }
}
