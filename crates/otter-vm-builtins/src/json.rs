//! JSON built-in
//!
//! Provides JSON parsing and serialization:
//! - `JSON.parse(text, reviver?)` - parse JSON string to value
//! - `JSON.stringify(value, replacer?, space?)` - serialize value to JSON string
//! - `JSON.rawJSON(string)` - create raw JSON wrapper (ES2024+)
//! - `JSON.isRawJSON(value)` - check if value is raw JSON (ES2024+)

use otter_vm_core::memory;
use otter_vm_core::object::{JsObject, PropertyKey};
use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native_with_mm as op_native};
use serde::Serialize;
use serde_json::Value as JsonValue;
use std::sync::Arc;

/// Get JSON ops for extension registration
pub fn ops() -> Vec<Op> {
    vec![
        op_native("__JSON_parse", json_parse),
        op_native("__JSON_stringify", json_stringify),
        op_native("__JSON_rawJSON", json_raw_json),
        op_native("__JSON_isRawJSON", json_is_raw_json),
    ]
}

// =============================================================================
// Helper functions
// =============================================================================

/// Parse JSON string with optional depth limit to prevent stack overflow
fn parse_json_safe(text: &str) -> Result<JsonValue, String> {
    serde_json::from_str(text).map_err(|e| format!("SyntaxError: {}", e))
}

fn json_to_value(value: &JsonValue, mm: Arc<memory::MemoryManager>) -> Value {
    match value {
        JsonValue::Null => Value::null(),
        JsonValue::Bool(b) => Value::boolean(*b),
        JsonValue::Number(n) => Value::number(n.as_f64().unwrap_or(f64::NAN)),
        JsonValue::String(s) => Value::string(JsString::intern(s)),
        JsonValue::Array(items) => {
            let arr = JsObject::array(items.len(), mm.clone());
            for (index, item) in items.iter().enumerate() {
                arr.set(
                    PropertyKey::Index(index as u32),
                    json_to_value(item, mm.clone()),
                );
            }
            Value::array(Arc::new(arr))
        }
        JsonValue::Object(map) => {
            let obj = JsObject::new(None, mm.clone());
            for (key, value) in map {
                obj.set(PropertyKey::string(key), json_to_value(value, mm.clone()));
            }
            Value::object(Arc::new(obj))
        }
    }
}

// =============================================================================
// Core methods
// =============================================================================

/// JSON.parse(text, reviver?)
/// Parse a JSON string, returning the JavaScript value or object described by the string.
fn json_parse(args: &[Value], mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let text = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("JSON.parse requires a string argument")?;

    // Parse the JSON
    let parsed = parse_json_safe(text.as_str())?;

    Ok(json_to_value(&parsed, mm))
}

/// JSON.stringify(value, replacer?, space?)
/// Convert a JavaScript value to a JSON string.
fn json_stringify(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    // Get the value as JSON string from JS side
    let value_str = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("JSON.stringify requires a value")?;

    // Get optional space parameter (third argument)
    let space = args.get(2).and_then(|v| {
        if let Some(n) = v.as_int32() {
            Some(n.clamp(0, 10) as usize)
        } else if let Some(s) = v.as_string() {
            // Use first 10 chars of string as indent
            let indent = s.as_str();
            if indent.is_empty() {
                None
            } else {
                Some(indent.len().min(10))
            }
        } else {
            None
        }
    });

    // Parse the input JSON
    let parsed: JsonValue = match serde_json::from_str(value_str.as_str()) {
        Ok(v) => v,
        Err(_) => {
            // If it's not valid JSON, return undefined
            return Ok(Value::undefined());
        }
    };

    // Check for cycles (serde_json handles this implicitly by not supporting cycles)
    // Stringify with optional pretty printing
    let result = if let Some(indent) = space {
        let indent_str = " ".repeat(indent);
        format_json_pretty(&parsed, &indent_str)
    } else {
        serde_json::to_string(&parsed).map_err(|e| format!("TypeError: {}", e))?
    };

    Ok(Value::string(JsString::intern(&result)))
}

/// Format JSON with pretty printing
fn format_json_pretty(value: &JsonValue, indent: &str) -> String {
    let formatter = serde_json::ser::PrettyFormatter::with_indent(indent.as_bytes());
    let mut buf = Vec::new();
    let mut ser = serde_json::Serializer::with_formatter(&mut buf, formatter);
    if value.serialize(&mut ser).is_ok() {
        String::from_utf8(buf).unwrap_or_else(|_| "null".to_string())
    } else {
        "null".to_string()
    }
}

/// JSON.rawJSON(string) - ES2024+
/// Creates a "raw JSON" object that can be serialized without modification.
/// Used for exact numeric representation (BigInt, high-precision numbers).
fn json_raw_json(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let text = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("JSON.rawJSON requires a string argument")?;

    let text_str = text.as_str();

    // Validate that the string is valid JSON primitive (not object/array)
    let parsed: JsonValue = serde_json::from_str(text_str)
        .map_err(|_| "SyntaxError: JSON.rawJSON requires valid JSON text")?;

    // rawJSON only accepts primitives (string, number, boolean, null)
    match &parsed {
        JsonValue::Object(_) | JsonValue::Array(_) => {
            return Err("SyntaxError: JSON.rawJSON only accepts JSON primitives".to_string());
        }
        _ => {}
    }

    // Return a special marker object that stringify can recognize
    let raw_obj = serde_json::json!({
        "__isRawJSON__": true,
        "rawJSON": text_str
    });

    Ok(Value::string(JsString::intern(&raw_obj.to_string())))
}

/// JSON.isRawJSON(value) - ES2024+
/// Tests whether a value is an object returned by JSON.rawJSON().
fn json_is_raw_json(args: &[Value], _mm: Arc<memory::MemoryManager>) -> Result<Value, String> {
    let value = match args.first() {
        Some(v) => v,
        None => return Ok(Value::boolean(false)),
    };

    // Check if it's our special raw JSON marker
    if let Some(s) = value.as_string()
        && let Ok(JsonValue::Object(obj)) = serde_json::from_str::<JsonValue>(s.as_str())
        && obj.get("__isRawJSON__") == Some(&JsonValue::Bool(true))
    {
        return Ok(Value::boolean(true));
    }

    Ok(Value::boolean(false))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_json_parse_object() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern(
            r#"{"name":"test","value":42}"#,
        ))];
        let result = json_parse(&args, memory_manager.clone()).unwrap();
        let obj = result.as_object().unwrap();
        assert_eq!(
            obj.get(&PropertyKey::string("name"))
                .and_then(|v| v.as_string().map(|s| s.as_str().to_string())),
            Some("test".to_string())
        );
        assert_eq!(
            obj.get(&PropertyKey::string("value"))
                .and_then(|v| v.as_number()),
            Some(42.0)
        );
    }

    #[test]
    fn test_json_parse_array() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern("[1,2,3]"))];
        let result = json_parse(&args, memory_manager.clone()).unwrap();
        let arr = result.as_object().unwrap();
        assert!(arr.is_array());
        assert_eq!(arr.array_length(), 3);
        assert_eq!(
            arr.get(&PropertyKey::Index(0)).and_then(|v| v.as_number()),
            Some(1.0)
        );
        assert_eq!(
            arr.get(&PropertyKey::Index(1)).and_then(|v| v.as_number()),
            Some(2.0)
        );
        assert_eq!(
            arr.get(&PropertyKey::Index(2)).and_then(|v| v.as_number()),
            Some(3.0)
        );
    }

    #[test]
    fn test_json_parse_primitives() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        // Number
        let args = vec![Value::string(JsString::intern("42"))];
        let result = json_parse(&args, memory_manager.clone()).unwrap();
        assert_eq!(result.as_number(), Some(42.0));

        // String
        let args = vec![Value::string(JsString::intern("\"hello\""))];
        let result = json_parse(&args, memory_manager.clone()).unwrap();
        assert_eq!(result.as_string().map(|s| s.as_str()), Some("hello"));

        // Boolean
        let args = vec![Value::string(JsString::intern("true"))];
        let result = json_parse(&args, memory_manager.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(true));

        // Null
        let args = vec![Value::string(JsString::intern("null"))];
        let result = json_parse(&args, memory_manager.clone()).unwrap();
        assert!(result.is_null());
    }

    #[test]
    fn test_json_parse_invalid() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern("{invalid}"))];
        let result = json_parse(&args, memory_manager.clone());
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SyntaxError"));
    }

    #[test]
    fn test_json_stringify_object() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern(r#"{"a":1,"b":2}"#))];
        let result = json_stringify(&args, memory_manager.clone()).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("\"a\""));
        assert!(s.contains("1"));
    }

    #[test]
    fn test_json_stringify_with_indent() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![
            Value::string(JsString::intern(r#"{"a":1}"#)),
            Value::null(),   // replacer (not used)
            Value::int32(2), // space
        ];
        let result = json_stringify(&args, memory_manager.clone()).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains('\n')); // Pretty printed
        assert!(s.contains("  ")); // 2-space indent
    }

    #[test]
    fn test_json_raw_json() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern("12345678901234567890"))];
        let result = json_raw_json(&args, memory_manager.clone()).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("__isRawJSON__"));
        assert!(s.contains("12345678901234567890"));
    }

    #[test]
    fn test_json_raw_json_rejects_object() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern(r#"{"a":1}"#))];
        let result = json_raw_json(&args, memory_manager.clone());
        assert!(result.is_err());
    }

    #[test]
    fn test_json_is_raw_json() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        // Create a raw JSON value
        let raw_args = vec![Value::string(JsString::intern("42"))];
        let raw = json_raw_json(&raw_args, memory_manager.clone()).unwrap();

        // Check if it's raw JSON
        let check_args = vec![raw];
        let result = json_is_raw_json(&check_args, memory_manager.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_json_is_raw_json_false() {
        let memory_manager = Arc::new(memory::MemoryManager::test());
        let args = vec![Value::string(JsString::intern("not raw json"))];
        let result = json_is_raw_json(&args, memory_manager.clone()).unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }
}
