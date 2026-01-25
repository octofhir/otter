//! Global object setup for JavaScript environment
//!
//! Provides the standard global functions and values:
//! - `globalThis` - reference to the global object itself
//! - `undefined`, `NaN`, `Infinity` - primitive values
//! - `eval`, `isFinite`, `isNaN`, `parseInt`, `parseFloat` - functions
//! - `encodeURI`, `decodeURI`, `encodeURIComponent`, `decodeURIComponent` - URI encoding

use std::sync::Arc;

use crate::object::{JsObject, PropertyKey};
use crate::string::JsString;
use crate::value::Value;

/// Set up all standard global properties on the global object
pub fn setup_global_object(global: &Arc<JsObject>) {
    // globalThis - self-referencing
    global.set(
        PropertyKey::string("globalThis"),
        Value::object(global.clone()),
    );

    // Primitive values
    global.set(PropertyKey::string("undefined"), Value::undefined());
    global.set(PropertyKey::string("NaN"), Value::number(f64::NAN));
    global.set(
        PropertyKey::string("Infinity"),
        Value::number(f64::INFINITY),
    );

    // Global functions
    global.set(
        PropertyKey::string("eval"),
        Value::native_function(global_eval),
    );
    global.set(
        PropertyKey::string("isFinite"),
        Value::native_function(global_is_finite),
    );
    global.set(
        PropertyKey::string("isNaN"),
        Value::native_function(global_is_nan),
    );
    global.set(
        PropertyKey::string("parseInt"),
        Value::native_function(global_parse_int),
    );
    global.set(
        PropertyKey::string("parseFloat"),
        Value::native_function(global_parse_float),
    );

    // URI encoding/decoding functions
    global.set(
        PropertyKey::string("encodeURI"),
        Value::native_function(global_encode_uri),
    );
    global.set(
        PropertyKey::string("decodeURI"),
        Value::native_function(global_decode_uri),
    );
    global.set(
        PropertyKey::string("encodeURIComponent"),
        Value::native_function(global_encode_uri_component),
    );
    global.set(
        PropertyKey::string("decodeURIComponent"),
        Value::native_function(global_decode_uri_component),
    );
}

// =============================================================================
// Global function implementations
// =============================================================================

/// Get argument at index, or undefined if missing
#[inline]
fn get_arg(args: &[Value], index: usize) -> Value {
    args.get(index).cloned().unwrap_or_default()
}

/// `eval(x)` - Evaluates JavaScript code represented as a string.
///
/// Note: Direct eval is not supported in this VM for security reasons.
/// Indirect eval throws an error.
fn global_eval(args: &[Value]) -> Result<Value, String> {
    // Per spec: if argument is not a string, return it unchanged
    let arg = get_arg(args, 0);

    if arg.is_string() {
        // eval() of a string is not supported in this VM
        Err("eval() is not supported".to_string())
    } else {
        // Non-string argument: return it unchanged
        Ok(arg)
    }
}

/// `isFinite(number)` - Determines whether the passed value is a finite number.
fn global_is_finite(args: &[Value]) -> Result<Value, String> {
    let value = get_arg(args, 0);
    let num = to_number(&value);
    Ok(Value::boolean(num.is_finite()))
}

/// `isNaN(number)` - Determines whether a value is NaN.
fn global_is_nan(args: &[Value]) -> Result<Value, String> {
    let value = get_arg(args, 0);
    let num = to_number(&value);
    Ok(Value::boolean(num.is_nan()))
}

/// `parseInt(string, radix)` - Parses a string and returns an integer.
fn global_parse_int(args: &[Value]) -> Result<Value, String> {
    let input = get_arg(args, 0);
    let radix_arg = args.get(1);

    // Convert input to string
    let input_str = to_string(&input);
    let trimmed = input_str.trim();

    if trimmed.is_empty() {
        return Ok(Value::number(f64::NAN));
    }

    // Determine sign
    let (sign, rest) = if let Some(s) = trimmed.strip_prefix('-') {
        (-1i64, s)
    } else if let Some(s) = trimmed.strip_prefix('+') {
        (1i64, s)
    } else {
        (1i64, trimmed)
    };

    // Determine radix
    let mut radix: u32 = match radix_arg {
        Some(r) => {
            let n = to_number(r) as i32;
            if n == 0 {
                10 // default
            } else if !(2..=36).contains(&n) {
                return Ok(Value::number(f64::NAN));
            } else {
                n as u32
            }
        }
        None => 10,
    };

    // Check for 0x/0X prefix
    let digits = if rest.len() >= 2 && (rest.starts_with("0x") || rest.starts_with("0X")) {
        if radix == 10 || radix == 16 {
            radix = 16;
            &rest[2..]
        } else {
            rest
        }
    } else {
        rest
    };

    if digits.is_empty() {
        return Ok(Value::number(f64::NAN));
    }

    // Parse digits one by one until we hit an invalid character
    let mut result: i64 = 0;
    let mut any_valid = false;

    for c in digits.chars() {
        let digit = match c.to_digit(radix) {
            Some(d) => d as i64,
            None => break, // Stop at first invalid character
        };
        any_valid = true;
        result = result.saturating_mul(radix as i64).saturating_add(digit);
    }

    if !any_valid {
        return Ok(Value::number(f64::NAN));
    }

    Ok(Value::number((sign * result) as f64))
}

/// `parseFloat(string)` - Parses a string and returns a floating point number.
fn global_parse_float(args: &[Value]) -> Result<Value, String> {
    let input = get_arg(args, 0);
    let input_str = to_string(&input);
    let trimmed = input_str.trim();

    if trimmed.is_empty() {
        return Ok(Value::number(f64::NAN));
    }

    // Handle special values
    if trimmed == "Infinity" || trimmed == "+Infinity" {
        return Ok(Value::number(f64::INFINITY));
    }
    if trimmed == "-Infinity" {
        return Ok(Value::number(f64::NEG_INFINITY));
    }

    // Find the longest valid prefix that parses as a number
    // Try progressively shorter prefixes until one parses.
    // We collect char indices to ensure we only slice at valid char boundaries.
    let mut indices: Vec<usize> = trimmed.char_indices().map(|(i, _)| i).collect();
    indices.push(trimmed.len());

    for &end in indices.iter().rev() {
        if end == 0 {
            continue;
        }
        let prefix = &trimmed[..end];
        if let Ok(n) = prefix.parse::<f64>() {
            return Ok(Value::number(n));
        }
    }

    Ok(Value::number(f64::NAN))
}

// =============================================================================
// URI encoding/decoding
// =============================================================================

/// Characters that encodeURI does NOT encode
const URI_UNESCAPED: &str =
    "ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_.!~*'()";
const URI_RESERVED: &str = ";/?:@&=+$,#";

/// `encodeURI(uri)` - Encodes a URI by replacing certain characters.
fn global_encode_uri(args: &[Value]) -> Result<Value, String> {
    let input = get_arg(args, 0);
    let uri = to_string(&input);

    let mut result = String::with_capacity(uri.len() * 3);

    for c in uri.chars() {
        if URI_UNESCAPED.contains(c) || URI_RESERVED.contains(c) {
            result.push(c);
        } else {
            // Encode the character as UTF-8 bytes
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }

    Ok(Value::string(JsString::intern(&result)))
}

/// `decodeURI(encodedURI)` - Decodes a URI previously created by encodeURI.
fn global_decode_uri(args: &[Value]) -> Result<Value, String> {
    let input = get_arg(args, 0);
    let encoded = to_string(&input);

    decode_uri_impl(&encoded, true)
}

/// `encodeURIComponent(str)` - Encodes a URI component.
fn global_encode_uri_component(args: &[Value]) -> Result<Value, String> {
    let input = get_arg(args, 0);
    let component = to_string(&input);

    let mut result = String::with_capacity(component.len() * 3);

    for c in component.chars() {
        if URI_UNESCAPED.contains(c) {
            result.push(c);
        } else {
            // Encode the character as UTF-8 bytes
            let mut buf = [0u8; 4];
            for byte in c.encode_utf8(&mut buf).bytes() {
                result.push_str(&format!("%{:02X}", byte));
            }
        }
    }

    Ok(Value::string(JsString::intern(&result)))
}

/// `decodeURIComponent(encodedURIComponent)` - Decodes a URI component.
fn global_decode_uri_component(args: &[Value]) -> Result<Value, String> {
    let input = get_arg(args, 0);
    let encoded = to_string(&input);

    decode_uri_impl(&encoded, false)
}

/// Common implementation for decodeURI and decodeURIComponent
fn decode_uri_impl(encoded: &str, preserve_reserved: bool) -> Result<Value, String> {
    let mut result = Vec::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            // Collect hex digits
            let mut hex_chars = String::with_capacity(2);
            for _ in 0..2 {
                match chars.next() {
                    Some(h) if h.is_ascii_hexdigit() => hex_chars.push(h),
                    _ => return Err("URIError: malformed URI sequence".to_string()),
                }
            }

            let byte = u8::from_str_radix(&hex_chars, 16)
                .map_err(|_| "URIError: malformed URI sequence".to_string())?;

            // For decodeURI, check if this is a reserved character
            if preserve_reserved && URI_RESERVED.contains(byte as char) && byte < 128 {
                // Keep the encoded form
                result.push(b'%');
                for b in hex_chars.bytes() {
                    result.push(b);
                }
            } else {
                result.push(byte);
            }
        } else {
            // Regular character: encode as UTF-8
            let mut buf = [0u8; 4];
            let encoded_char = c.encode_utf8(&mut buf);
            result.extend_from_slice(encoded_char.as_bytes());
        }
    }

    // Convert bytes to string
    let decoded =
        String::from_utf8(result).map_err(|_| "URIError: malformed URI sequence".to_string())?;

    Ok(Value::string(JsString::intern(&decoded)))
}

// =============================================================================
// Type conversion helpers
// =============================================================================

/// Convert a Value to a number (ToNumber abstract operation)
fn to_number(value: &Value) -> f64 {
    if let Some(n) = value.as_number() {
        return n;
    }

    if value.is_undefined() {
        return f64::NAN;
    }

    if value.is_null() {
        return 0.0;
    }

    if let Some(b) = value.as_boolean() {
        return if b { 1.0 } else { 0.0 };
    }

    if let Some(s) = value.as_string() {
        let trimmed = s.as_str().trim();
        if trimmed.is_empty() {
            return 0.0;
        }
        trimmed.parse::<f64>().unwrap_or(f64::NAN)
    } else {
        f64::NAN
    }
}

/// Convert a Value to a string (ToString abstract operation)
fn to_string(value: &Value) -> String {
    if let Some(s) = value.as_string() {
        return s.as_str().to_string();
    }

    if value.is_undefined() {
        return "undefined".to_string();
    }

    if value.is_null() {
        return "null".to_string();
    }

    if let Some(b) = value.as_boolean() {
        return if b { "true" } else { "false" }.to_string();
    }

    if let Some(n) = value.as_number() {
        if n.is_nan() {
            return "NaN".to_string();
        }
        if n.is_infinite() {
            return if n.is_sign_positive() {
                "Infinity"
            } else {
                "-Infinity"
            }
            .to_string();
        }
        // Format number
        let formatted = if n.fract() == 0.0 && n.abs() < 1e15 {
            format!("{}", n as i64)
        } else {
            format!("{}", n)
        };
        return formatted;
    }

    "[object Object]".to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_this_setup() {
        let global = Arc::new(JsObject::new(None));
        setup_global_object(&global);

        // globalThis should reference the global object itself
        let global_this = global.get(&PropertyKey::string("globalThis"));
        assert!(global_this.is_some());

        // The globalThis value should be an object
        let gt = global_this.unwrap();
        assert!(gt.is_object());
    }

    #[test]
    fn test_is_finite() {
        // Finite numbers
        assert_eq!(
            global_is_finite(&[Value::number(42.0)])
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            global_is_finite(&[Value::number(0.0)])
                .unwrap()
                .as_boolean(),
            Some(true)
        );

        // Non-finite
        assert_eq!(
            global_is_finite(&[Value::number(f64::INFINITY)])
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            global_is_finite(&[Value::number(f64::NEG_INFINITY)])
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            global_is_finite(&[Value::number(f64::NAN)])
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_is_nan() {
        assert_eq!(
            global_is_nan(&[Value::number(f64::NAN)])
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            global_is_nan(&[Value::number(42.0)]).unwrap().as_boolean(),
            Some(false)
        );
        assert_eq!(
            global_is_nan(&[Value::undefined()]).unwrap().as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn test_parse_int() {
        // Basic integers
        assert_eq!(
            global_parse_int(&[Value::string(JsString::intern("42"))])
                .unwrap()
                .as_number(),
            Some(42.0)
        );
        assert_eq!(
            global_parse_int(&[Value::string(JsString::intern("-123"))])
                .unwrap()
                .as_number(),
            Some(-123.0)
        );
        assert_eq!(
            global_parse_int(&[Value::string(JsString::intern("+456"))])
                .unwrap()
                .as_number(),
            Some(456.0)
        );

        // With radix
        assert_eq!(
            global_parse_int(&[Value::string(JsString::intern("ff")), Value::number(16.0)])
                .unwrap()
                .as_number(),
            Some(255.0)
        );
        assert_eq!(
            global_parse_int(&[Value::string(JsString::intern("1010")), Value::number(2.0)])
                .unwrap()
                .as_number(),
            Some(10.0)
        );

        // Hex prefix
        assert_eq!(
            global_parse_int(&[Value::string(JsString::intern("0xFF"))])
                .unwrap()
                .as_number(),
            Some(255.0)
        );

        // Stops at invalid char
        assert_eq!(
            global_parse_int(&[Value::string(JsString::intern("123abc"))])
                .unwrap()
                .as_number(),
            Some(123.0)
        );

        // Invalid - returns NaN
        let result = global_parse_int(&[Value::string(JsString::intern("hello"))]).unwrap();
        assert!(result.is_nan());
        assert!(result.as_number().unwrap().is_nan());
    }

    #[test]
    fn test_parse_float() {
        assert_eq!(
            global_parse_float(&[Value::string(JsString::intern("3.5"))])
                .unwrap()
                .as_number(),
            Some(3.5)
        );
        assert_eq!(
            global_parse_float(&[Value::string(JsString::intern("-2.5"))])
                .unwrap()
                .as_number(),
            Some(-2.5)
        );
        assert_eq!(
            global_parse_float(&[Value::string(JsString::intern("  42  "))])
                .unwrap()
                .as_number(),
            Some(42.0)
        );
        assert_eq!(
            global_parse_float(&[Value::string(JsString::intern("Infinity"))])
                .unwrap()
                .as_number(),
            Some(f64::INFINITY)
        );
    }

    #[test]
    fn test_encode_uri_component() {
        let result =
            global_encode_uri_component(&[Value::string(JsString::intern("hello world"))]).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");

        let result =
            global_encode_uri_component(&[Value::string(JsString::intern("a=1&b=2"))]).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "a%3D1%26b%3D2");
    }

    #[test]
    fn test_decode_uri_component() {
        let result =
            global_decode_uri_component(&[Value::string(JsString::intern("hello%20world"))])
                .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello world");

        let result =
            global_decode_uri_component(&[Value::string(JsString::intern("a%3D1%26b%3D2"))])
                .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "a=1&b=2");
    }

    #[test]
    fn test_encode_uri() {
        // encodeURI does not encode reserved characters
        let result = global_encode_uri(&[Value::string(JsString::intern(
            "http://example.com/path?q=1",
        ))])
        .unwrap();
        assert_eq!(
            result.as_string().unwrap().as_str(),
            "http://example.com/path?q=1"
        );

        // But does encode other special chars
        let result = global_encode_uri(&[Value::string(JsString::intern("hello world"))]).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");
    }

    #[test]
    fn test_decode_uri() {
        let result =
            global_decode_uri(&[Value::string(JsString::intern("hello%20world"))]).unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello world");
    }

    #[test]
    fn test_eval_non_string() {
        // eval with non-string returns the value unchanged
        assert_eq!(
            global_eval(&[Value::number(42.0)]).unwrap().as_number(),
            Some(42.0)
        );
        assert!(global_eval(&[Value::undefined()]).unwrap().is_undefined());
    }

    #[test]
    fn test_eval_string() {
        // eval with string is not supported
        let result = global_eval(&[Value::string(JsString::intern("1 + 1"))]);
        assert!(result.is_err());
    }
}
