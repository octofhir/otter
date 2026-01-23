//! JSON built-in
//!
//! Provides JSON parsing and serialization:
//! - `JSON.parse(text, reviver?)` - parse JSON string to value
//! - `JSON.stringify(value, replacer?, space?)` - serialize value to JSON string
//! - `JSON.rawJSON(string)` - create raw JSON wrapper (ES2024+)
//! - `JSON.isRawJSON(value)` - check if value is raw JSON (ES2024+)

use otter_vm_core::string::JsString;
use otter_vm_core::value::Value;
use otter_vm_runtime::{Op, op_native};
use serde::Serialize;
use serde_json::Value as JsonValue;

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

/// Convert serde_json::Value to a JS-compatible JSON string
fn json_value_to_string(value: &JsonValue) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "null".to_string())
}

/// Parse JSON string with optional depth limit to prevent stack overflow
fn parse_json_safe(text: &str) -> Result<JsonValue, String> {
    serde_json::from_str(text).map_err(|e| format!("SyntaxError: {}", e))
}

// =============================================================================
// Core methods
// =============================================================================

/// JSON.parse(text, reviver?)
/// Parse a JSON string, returning the JavaScript value or object described by the string.
fn json_parse(args: &[Value]) -> Result<Value, String> {
    let text = args
        .first()
        .and_then(|v| v.as_string())
        .ok_or("JSON.parse requires a string argument")?;

    // Parse the JSON
    let parsed = parse_json_safe(text.as_str())?;

    // Convert back to JSON string for JS side to handle
    // The JS wrapper will convert this to actual JS values
    let result = json_value_to_string(&parsed);
    Ok(Value::string(JsString::intern(&result)))
}

/// JSON.stringify(value, replacer?, space?)
/// Convert a JavaScript value to a JSON string.
fn json_stringify(args: &[Value]) -> Result<Value, String> {
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
fn json_raw_json(args: &[Value]) -> Result<Value, String> {
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
fn json_is_raw_json(args: &[Value]) -> Result<Value, String> {
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
        let args = vec![Value::string(JsString::intern(
            r#"{"name":"test","value":42}"#,
        ))];
        let result = json_parse(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("name"));
        assert!(s.contains("test"));
        assert!(s.contains("42"));
    }

    #[test]
    fn test_json_parse_array() {
        let args = vec![Value::string(JsString::intern("[1,2,3]"))];
        let result = json_parse(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert_eq!(s, "[1,2,3]");
    }

    #[test]
    fn test_json_parse_primitives() {
        // Number
        let args = vec![Value::string(JsString::intern("42"))];
        let result = json_parse(&args).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "42");

        // String
        let args = vec![Value::string(JsString::intern("\"hello\""))];
        let result = json_parse(&args).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "\"hello\"");

        // Boolean
        let args = vec![Value::string(JsString::intern("true"))];
        let result = json_parse(&args).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "true");

        // Null
        let args = vec![Value::string(JsString::intern("null"))];
        let result = json_parse(&args).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "null");
    }

    #[test]
    fn test_json_parse_invalid() {
        let args = vec![Value::string(JsString::intern("{invalid}"))];
        let result = json_parse(&args);
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("SyntaxError"));
    }

    #[test]
    fn test_json_stringify_object() {
        let args = vec![Value::string(JsString::intern(r#"{"a":1,"b":2}"#))];
        let result = json_stringify(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("\"a\""));
        assert!(s.contains("1"));
    }

    #[test]
    fn test_json_stringify_with_indent() {
        let args = vec![
            Value::string(JsString::intern(r#"{"a":1}"#)),
            Value::null(),   // replacer (not used)
            Value::int32(2), // space
        ];
        let result = json_stringify(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains('\n')); // Pretty printed
        assert!(s.contains("  ")); // 2-space indent
    }

    #[test]
    fn test_json_raw_json() {
        let args = vec![Value::string(JsString::intern("12345678901234567890"))];
        let result = json_raw_json(&args).unwrap();
        let s = result.as_string().unwrap().to_string();
        assert!(s.contains("__isRawJSON__"));
        assert!(s.contains("12345678901234567890"));
    }

    #[test]
    fn test_json_raw_json_rejects_object() {
        let args = vec![Value::string(JsString::intern(r#"{"a":1}"#))];
        let result = json_raw_json(&args);
        assert!(result.is_err());
    }

    #[test]
    fn test_json_is_raw_json() {
        // Create a raw JSON value
        let raw_args = vec![Value::string(JsString::intern("42"))];
        let raw = json_raw_json(&raw_args).unwrap();

        // Check if it's raw JSON
        let check_args = vec![raw];
        let result = json_is_raw_json(&check_args).unwrap();
        assert_eq!(result.as_boolean(), Some(true));
    }

    #[test]
    fn test_json_is_raw_json_false() {
        let args = vec![Value::string(JsString::intern("not raw json"))];
        let result = json_is_raw_json(&args).unwrap();
        assert_eq!(result.as_boolean(), Some(false));
    }
}
