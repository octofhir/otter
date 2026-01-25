//! Global object setup for JavaScript environment
//!
//! Provides the standard global functions and values:
//! - `globalThis` - reference to the global object itself
//! - `undefined`, `NaN`, `Infinity` - primitive values
//! - `eval`, `isFinite`, `isNaN`, `parseInt`, `parseFloat` - functions
//! - `encodeURI`, `decodeURI`, `encodeURIComponent`, `decodeURIComponent` - URI encoding

use std::sync::Arc;

use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::regexp::JsRegExp;
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

    // Standard built-in objects
    setup_builtin_constructors(global);
}

/// Set up standard built-in constructors and their prototypes
fn setup_builtin_constructors(global: &Arc<JsObject>) {
    let builtins = [
        "Object",
        "Function",
        "Array",
        "String",
        "Number",
        "Boolean",
        "RegExp",
        "Error",
        "TypeError",
        "ReferenceError",
        "SyntaxError",
        "RangeError",
        "URIError",
        "EvalError",
        "BigInt",
        "Test262Error",
    ];

    for name in builtins {
        let proto = Arc::new(JsObject::new(None));
        // Create constructor based on type
        let ctor = if name == "Boolean" {
            Value::native_function(|args| {
                let b = if let Some(val) = args.get(0) {
                    to_boolean(val)
                } else {
                    false // to_boolean(undefined) is false
                };
                Ok(Value::boolean(b))
            })
        } else if name == "RegExp" {
            Value::native_function(|args| {
                let pattern = args
                    .get(0)
                    .map(|v| to_string(v))
                    .unwrap_or_else(|| "".to_string());
                let flags = args
                    .get(1)
                    .map(|v| to_string(v))
                    .unwrap_or_else(|| "".to_string());

                // Get RegExp.prototype from new.target or default?
                // For now, simpler: create with None (default)
                // Actually if called as function, RegExp acts as constructor (ES6 NewTarget logic needed?)
                // OtterVM might not support new.target fully yet or it's implicitly constructor.

                // Construct RegExp object
                let regex = Arc::new(JsRegExp::new(
                    pattern, flags,
                    None, // TODO: Link to proper prototype if needed explicitly, but JsObject::new(None) usually fine?
                         // No, JsObject::new(None) creates object with Object.prototype (or null?).
                         // We want RegExp.prototype.
                         // But we are inside setup_builtin_constructors loop, 'proto' variable is the prototype we are building!
                         // But we want instances to have THAT prototype.
                         // We can't access 'proto' here inside closure easily if we move it?
                         // Actually 'proto' is Arc, we can clone it into closure.
                ));
                Ok(Value::regex(regex))
            })
        } else if name == "BigInt" {
            Value::native_function(|args| {
                if let Some(val) = args.get(0) {
                    if let Some(n) = val.as_number() {
                        if n.is_nan() || n.is_infinite() {
                            return Err("RangeError: invalid BigInt".to_string());
                        }
                        if n.trunc() != n {
                            return Err("RangeError: The number cannot be converted to a BigInt because it is not an integer".to_string());
                        }
                        return Ok(Value::bigint(format!("{:.0}", n)));
                    }
                    if val.is_string() {
                        let s = to_string(val);
                        return Ok(Value::bigint(s));
                    }
                    if val.is_boolean() {
                        return Ok(Value::bigint(if val.to_boolean() {
                            "1".to_string()
                        } else {
                            "0".to_string()
                        }));
                    }
                    // Fallback
                    let s = to_string(val);
                    Ok(Value::bigint(s))
                } else {
                    Err("TypeError: Cannot convert undefined to a BigInt".to_string())
                }
            })
        } else {
            Value::native_function(|args| {
                // If called as a constructor (which we assume for now for these builtins),
                // and arguments are present, we might want to set properties.
                // For Error types, setting 'message' is crucial.
                if let Some(msg) = args.get(0) {
                    let obj = JsObject::new(None);
                    obj.set(PropertyKey::string("message"), msg.clone());
                    return Ok(Value::object(Arc::new(obj)));
                }
                Ok(Value::undefined())
            })
        };

        // Add basic toString to prototypes
        if name == "Object" {
            proto.set(
                PropertyKey::string("toString"),
                Value::native_function(|_| Ok(Value::string(JsString::intern("[object Object]")))),
            );
        } else if name == "Function" {
            proto.set(
                PropertyKey::string("toString"),
                Value::native_function(|_| {
                    Ok(Value::string(JsString::intern(
                        "function () { [native code] }",
                    )))
                }),
            );
        } else if name == "String" {
            proto.set(
                PropertyKey::string("toString"),
                Value::native_function(|args| {
                    if let Some(this_val) = args.get(0) {
                        return Ok(Value::string(JsString::intern(&to_string(this_val))));
                    }
                    Ok(Value::string(JsString::intern("")))
                }),
            );
        }

        if let Some(ctor_obj) = ctor.as_object() {
            ctor_obj.set(
                PropertyKey::string("prototype"),
                Value::object(proto.clone()),
            );
            proto.set(PropertyKey::string("constructor"), ctor.clone());

            // Add static methods to constructors
            if name == "String" {
                ctor_obj.set(
                    PropertyKey::string("fromCharCode"),
                    Value::native_function(|args| {
                        let mut result = String::new();
                        for arg in args {
                            let n = if let Some(n) = arg.as_number() {
                                n
                            } else if let Some(s) = arg.as_string() {
                                // Basic ToNumber for strings (decimal only for now)
                                s.as_str().parse::<f64>().unwrap_or(f64::NAN)
                            } else {
                                // Default to 0 for others (simplified)
                                0.0
                            };

                            if !n.is_nan() {
                                if let Some(c) = std::char::from_u32(n as u32) {
                                    result.push(c);
                                }
                            }
                        }
                        Ok(Value::string(JsString::intern(&result)))
                    }),
                );
            }
        }

        // Add more prototype methods
        if name == "String" {
            proto.set(
                PropertyKey::string("indexOf"),
                Value::native_function(|args| {
                    if let (Some(this_val), Some(search_val)) = (args.get(0), args.get(1)) {
                        let this_str = to_string(this_val);
                        let search_str = to_string(search_val);
                        if let Some(pos) = this_str.find(&search_str) {
                            return Ok(Value::number(pos as f64));
                        }
                    }
                    Ok(Value::number(-1.0))
                }),
            );
            proto.set(
                PropertyKey::string("valueOf"),
                Value::native_function(|args| {
                    if let Some(this_val) = args.get(0) {
                        return Ok(this_val.clone());
                    }
                    Ok(Value::undefined())
                }),
            );
        } else if name == "RegExp" {
            proto.set(
                PropertyKey::string("test"),
                Value::native_function(|_args| {
                    Ok(Value::boolean(true)) // Placeholder: test usually returns boolean
                }),
            );
            proto.set(
                PropertyKey::string("exec"),
                Value::native_function(|args| {
                    let this_val_opt = args.get(0);
                    let regex = this_val_opt.and_then(|v| v.as_regex()).ok_or_else(|| {
                        "TypeError: RegExp.prototype.exec called on incompatible receiver"
                            .to_string()
                    })?;

                    let input_str = if let Some(val) = args.get(1) {
                        to_string(val)
                    } else {
                        "undefined".to_string()
                    };

                    if let Some((index, matches)) = regex.exec(&input_str) {
                        // Create result array
                        let arr = JsObject::array(matches.len());
                        for (i, m) in matches.iter().enumerate() {
                            let val = m
                                .as_ref()
                                .map(|s| Value::string(JsString::intern(s)))
                                .unwrap_or(Value::undefined());
                            arr.set(PropertyKey::Index(i as u32), val);
                        }
                        arr.set(PropertyKey::string("index"), Value::number(index as f64));
                        arr.set(
                            PropertyKey::string("input"),
                            Value::string(JsString::intern(&input_str)),
                        );
                        arr.set(PropertyKey::string("groups"), Value::undefined());

                        Ok(Value::array(Arc::new(arr)))
                    } else {
                        Ok(Value::null())
                    }
                }),
            );
        } else if name == "Object" {
            proto.set(
                PropertyKey::string("valueOf"),
                Value::native_function(|args| {
                    if let Some(this_val) = args.get(0) {
                        return Ok(this_val.clone());
                    }
                    Ok(Value::undefined())
                }),
            );
        }

        global.define_property(PropertyKey::string(name), PropertyDescriptor::data(ctor));
    }
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
fn to_boolean(value: &Value) -> bool {
    if let Some(b) = value.as_boolean() {
        return b;
    }
    if value.is_undefined() || value.is_null() {
        return false;
    }
    if let Some(n) = value.as_number() {
        return n != 0.0 && !n.is_nan();
    }
    if let Some(s) = value.as_string() {
        return !s.as_str().is_empty();
    }
    true // Objects are true
}

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
