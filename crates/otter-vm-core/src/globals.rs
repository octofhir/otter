//! Global object setup for JavaScript environment
//!
//! Provides the standard global functions and values:
//! - `globalThis` - reference to the global object itself
//! - `undefined`, `NaN`, `Infinity` - primitive values
//! - `eval`, `isFinite`, `isNaN`, `parseInt`, `parseFloat` - functions
//! - `encodeURI`, `decodeURI`, `encodeURIComponent`, `decodeURIComponent` - URI encoding

use std::sync::Arc;

use crate::array_buffer::JsArrayBuffer;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;

/// Set up all standard global properties on the global object.
///
/// `fn_proto` is the intrinsic `%Function.prototype%` created by VmRuntime.
/// All native functions receive it as their `[[Prototype]]` per ES2023 §10.3.1.
pub fn setup_global_object(global: GcRef<JsObject>, fn_proto: GcRef<JsObject>) {
    let mm = global.memory_manager().clone();

    // globalThis - self-referencing
    global.set(PropertyKey::string("globalThis"), Value::object(global));

    // Primitive values
    global.set(PropertyKey::string("undefined"), Value::undefined());
    global.set(PropertyKey::string("NaN"), Value::number(f64::NAN));
    global.set(
        PropertyKey::string("Infinity"),
        Value::number(f64::INFINITY),
    );

    // Global functions — all get fn_proto as [[Prototype]]
    global.set(
        PropertyKey::string("eval"),
        Value::native_function_with_proto(global_eval, mm.clone(), fn_proto),
    );
    global.set(
        PropertyKey::string("isFinite"),
        Value::native_function_with_proto(global_is_finite, mm.clone(), fn_proto),
    );
    global.set(
        PropertyKey::string("isNaN"),
        Value::native_function_with_proto(global_is_nan, mm.clone(), fn_proto),
    );
    global.set(
        PropertyKey::string("parseInt"),
        Value::native_function_with_proto(global_parse_int, mm.clone(), fn_proto),
    );
    global.set(
        PropertyKey::string("parseFloat"),
        Value::native_function_with_proto(global_parse_float, mm.clone(), fn_proto),
    );

    // URI encoding/decoding functions
    global.set(
        PropertyKey::string("encodeURI"),
        Value::native_function_with_proto(global_encode_uri, mm.clone(), fn_proto),
    );
    global.set(
        PropertyKey::string("decodeURI"),
        Value::native_function_with_proto(global_decode_uri, mm.clone(), fn_proto),
    );
    global.set(
        PropertyKey::string("encodeURIComponent"),
        Value::native_function_with_proto(global_encode_uri_component, mm.clone(), fn_proto),
    );
    global.set(
        PropertyKey::string("decodeURIComponent"),
        Value::native_function_with_proto(global_decode_uri_component, mm.clone(), fn_proto),
    );

    // Standard built-in objects
    setup_builtin_constructors(global, fn_proto);
}

/// Set up standard built-in constructors and their prototypes.
/// `fn_proto` is the intrinsic `%Function.prototype%` — used as-is for `Function.prototype`
/// and as `[[Prototype]]` for all native function objects.
fn setup_builtin_constructors(global: GcRef<JsObject>, fn_proto: GcRef<JsObject>) {
    let mm = global.memory_manager().clone();
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
        "Date",
        "BigInt",
        "Test262Error",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "Promise",
        "Proxy",
        "Symbol",
        "GeneratorPrototype",
        "IteratorPrototype",
        "AsyncIteratorPrototype",
        "AsyncGeneratorPrototype",
        "ArrayBuffer",
        "DataView",
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float32Array",
        "Float64Array",
        "BigInt64Array",
        "BigUint64Array",
    ];

    for name in builtins {
        // For the "Function" constructor, use the intrinsic fn_proto
        // instead of creating a fresh object. This is the BOA/V8 pattern:
        // Function.prototype is created once and shared.
        let proto = if name == "Function" {
            fn_proto
        } else {
            GcRef::new(JsObject::new(None, mm.clone()))
        };

        // Create constructor based on type — all get fn_proto as [[Prototype]]
        let ctor = if name == "Boolean" {
            Value::native_function_with_proto(
                |_this, args: &[Value], _mm| {
                    let b = if let Some(val) = args.get(0) {
                        to_boolean(val)
                    } else {
                        false // to_boolean(undefined) is false
                    };
                    Ok(Value::boolean(b))
                },
                mm.clone(),
                fn_proto,
            )
        } else if name == "BigInt" {
            Value::native_function_with_proto(
                |_this, args: &[Value], _mm| {
                    if let Some(val) = args.get(0) {
                        if let Some(n) = val.as_number() {
                            if n.is_nan() || n.is_infinite() {
                                return Err(VmError::type_error("RangeError: invalid BigInt"));
                            }
                            if n.trunc() != n {
                                return Err(VmError::type_error("RangeError: The number cannot be converted to a BigInt because it is not an integer"));
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
                        Err(VmError::type_error("TypeError: Cannot convert undefined to a BigInt"))
                    }
                },
                mm.clone(),
                fn_proto,
            )
        } else if name == "ArrayBuffer" {
            let mm_clone = mm.clone();
            Value::native_function_with_proto(
                move |_this, args: &[Value], mm_inner| {
                    let len = if let Some(arg) = args.get(0) {
                        let n = to_number(arg);
                        if n.is_nan() {
                            0
                        } else {
                            n as usize
                        }
                    } else {
                        0
                    };

                    let ab = Arc::new(JsArrayBuffer::new(len, Some(fn_proto), mm_inner));
                    Ok(Value::array_buffer(ab))
                },
                mm_clone,
                fn_proto,
            )
        } else {
            let mm_clone = mm.clone();
            Value::native_function_with_proto(
                move |_this, args: &[Value], mm_inner| {
                    // If called as a constructor (which we assume for now for these builtins),
                    // and arguments are present, we might want to set properties.
                    // For Error types, setting 'message' is crucial.
                    if let Some(msg) = args.get(0) {
                        let obj = JsObject::new(None, mm_inner);
                        obj.set(PropertyKey::string("message"), msg.clone());
                        return Ok(Value::object(GcRef::new(obj)));
                    }
                    Ok(Value::undefined())
                },
                mm_clone,
                fn_proto,
            )
        };

        // Add basic toString to prototypes
        if name == "Object" {
            proto.set(
                PropertyKey::string("toString"),
                Value::native_function_with_proto(
                    |_this, _, _mm| Ok(Value::string(JsString::intern("[object Object]"))),
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "Function" {
            proto.set(
                PropertyKey::string("toString"),
                Value::native_function_with_proto(
                    |_this, _, _mm| {
                        Ok(Value::string(JsString::intern(
                            "function () { [native code] }",
                        )))
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "String" {
            proto.set(
                PropertyKey::string("toString"),
                Value::native_function_with_proto(
                    |this_val, _args, _mm| {
                        Ok(Value::string(JsString::intern(&to_string(this_val))))
                    },
                    mm.clone(),
                    fn_proto,
                ),
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
                    Value::native_function_with_proto(
                        |_this, args: &[Value], _mm| {
                            let mut result = String::new();
                            for arg in args {
                                // Per ES2023 §22.1.2.1: ToUint16(ToNumber(arg))
                                let n = if let Some(n) = arg.as_number() {
                                    n
                                } else if let Some(i) = arg.as_int32() {
                                    i as f64
                                } else if let Some(s) = arg.as_string() {
                                    let trimmed = s.as_str().trim();
                                    if trimmed.is_empty() {
                                        0.0
                                    } else {
                                        trimmed.parse::<f64>().unwrap_or(f64::NAN)
                                    }
                                } else if let Some(b) = arg.as_boolean() {
                                    if b { 1.0 } else { 0.0 }
                                } else if arg.is_null() {
                                    0.0
                                } else {
                                    f64::NAN
                                };
                                let code = if n.is_nan() || n.is_infinite() {
                                    0u16
                                } else {
                                    (n.trunc() as i64 as u32 & 0xFFFF) as u16
                                };
                                if let Some(c) = std::char::from_u32(code as u32) {
                                    result.push(c);
                                }
                            }
                            Ok(Value::string(JsString::intern(&result)))
                        },
                        mm.clone(),
                        fn_proto,
                    ),
                );
            } else if name == "ArrayBuffer" {
                ctor_obj.set(
                    PropertyKey::string("isView"),
                    Value::native_function_with_proto(
                        |_this, args, _mm| {
                            if let Some(arg) = args.get(0) {
                                Ok(Value::boolean(arg.is_typed_array() || arg.is_data_view()))
                            } else {
                                Ok(Value::boolean(false))
                            }
                        },
                        mm.clone(),
                        fn_proto,
                    ),
                );
            }
        }

        // Add more prototype methods
        if name == "String" {
            proto.set(
                PropertyKey::string("indexOf"),
                Value::native_function_with_proto(
                    |this_val, args, _mm| {
                        if let Some(search_val) = args.get(0) {
                            let this_str = to_string(this_val);
                            let search_str = to_string(search_val);
                            if let Some(pos) = this_str.find(&search_str) {
                                return Ok(Value::number(pos as f64));
                            }
                        }
                        Ok(Value::number(-1.0))
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
            proto.set(
                PropertyKey::string("valueOf"),
                Value::native_function_with_proto(
                    |this_val, _args, _mm| {
                        Ok::<Value, VmError>(this_val.clone())
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "Object" {
            proto.set(
                PropertyKey::string("valueOf"),
                Value::native_function_with_proto(
                    |this_val, _args, _mm| {
                        Ok::<Value, VmError>(this_val.clone())
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "ArrayBuffer" {
            // ArrayBuffer.prototype.byteLength getter
            proto.define_property(
                PropertyKey::string("byteLength"),
                PropertyDescriptor::getter(Value::native_function_with_proto(
                    |this_val, args, _mm| {
                        if let Some(this) = this_val.as_array_buffer() {
                            Ok(Value::number(this.byte_length() as f64))
                        } else {
                             Err(VmError::type_error("TypeError: ArrayBuffer.prototype.byteLength called on incompatible receiver"))
                        }
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );

            // ArrayBuffer.prototype.slice
            proto.set(
                PropertyKey::string("slice"),
                Value::native_function_with_proto(
                    |this_val, args, _mm| {
                        let ab = this_val.as_array_buffer()
                            .ok_or("TypeError: ArrayBuffer.prototype.slice called on incompatible receiver")?;
                        
                        let len = ab.byte_length() as f64;
                        
                        let start_arg = to_number(args.get(0).unwrap_or(&Value::undefined()));
                        let start = if start_arg.is_nan() {
                            0
                        } else if start_arg < 0.0 {
                            (len + start_arg).max(0.0) as usize
                        } else {
                            start_arg.min(len) as usize
                        };

                        let end_arg = args.get(1).map(to_number).unwrap_or(len);
                        let end = if end_arg.is_nan() {
                            0
                        } else if end_arg < 0.0 {
                            (len + end_arg).max(0.0) as usize
                        } else {
                            end_arg.min(len) as usize
                        };

                        let new_ab = ab.slice(start, end).ok_or("Failed to slice ArrayBuffer")?;
                        Ok(Value::array_buffer(Arc::new(new_ab)))
                    },
                    mm.clone(),
                    fn_proto,
                ),
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
/// For indirect eval (`var e = eval; e("...")` or `(0, eval)("...")`),
/// this native function is called. It signals the interpreter via
/// `InterceptionSignal::EvalCall` so the VM can compile and execute
/// the code with full context access.
fn global_eval(_this: &Value, args: &[Value], _mm: Arc<crate::memory::MemoryManager>) -> Result<Value, VmError> {
    // Per spec: if argument is not a string, return it unchanged
    let arg = get_arg(args, 0);

    if arg.is_string() {
        // Signal the interpreter to handle eval with full VM context
        Err(VmError::interception(crate::error::InterceptionSignal::EvalCall))
    } else {
        // Non-string argument: return it unchanged (per spec §19.2.1.1)
        Ok(arg)
    }
}

/// `isFinite(number)` - Determines whether the passed value is a finite number.
fn global_is_finite(
    _this: &Value,
    args: &[Value],
    _mm: Arc<crate::memory::MemoryManager>,
) -> Result<Value, VmError> {
    let value = get_arg(args, 0);
    let num = to_number(&value);
    Ok(Value::boolean(num.is_finite()))
}

/// `isNaN(number)` - Determines whether a value is NaN.
fn global_is_nan(_this: &Value, args: &[Value], _mm: Arc<crate::memory::MemoryManager>) -> Result<Value, VmError> {
    let value = get_arg(args, 0);
    let num = to_number(&value);
    Ok(Value::boolean(num.is_nan()))
}

/// `parseInt(string, radix)` - Parses a string and returns an integer.
fn global_parse_int(
    _this: &Value,
    args: &[Value],
    _mm: Arc<crate::memory::MemoryManager>,
) -> Result<Value, VmError> {
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
fn global_parse_float(
    _this: &Value,
    args: &[Value],
    _mm: Arc<crate::memory::MemoryManager>,
) -> Result<Value, VmError> {
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
fn global_encode_uri(
    _this: &Value,
    args: &[Value],
    _mm: Arc<crate::memory::MemoryManager>,
) -> Result<Value, VmError> {
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
fn global_decode_uri(
    _this: &Value,
    args: &[Value],
    _mm: Arc<crate::memory::MemoryManager>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let encoded = to_string(&input);

    decode_uri_impl(&encoded, true)
}

/// `encodeURIComponent(str)` - Encodes a URI component.
fn global_encode_uri_component(
    _this: &Value,
    args: &[Value],
    _mm: Arc<crate::memory::MemoryManager>,
) -> Result<Value, VmError> {
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
fn global_decode_uri_component(
    _this: &Value,
    args: &[Value],
    _mm: Arc<crate::memory::MemoryManager>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let encoded = to_string(&input);

    decode_uri_impl(&encoded, false)
}

/// Common implementation for decodeURI and decodeURIComponent
fn decode_uri_impl(encoded: &str, preserve_reserved: bool) -> Result<Value, VmError> {
    let mut result = Vec::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            // Collect hex digits
            let mut hex_chars = String::with_capacity(2);
            for _ in 0..2 {
                match chars.next() {
                    Some(h) if h.is_ascii_hexdigit() => hex_chars.push(h),
                    _ => return Err(VmError::type_error("URIError: malformed URI sequence")),
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
pub fn to_number(value: &Value) -> f64 {
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

pub fn to_string(value: &Value) -> String {
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

fn to_js_string(value: &Value) -> GcRef<JsString> {
    if let Some(s) = value.as_string() {
        return s;
    }
    JsString::intern(&to_string(value))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_global_this_setup() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let global = GcRef::new(JsObject::new(None, memory_manager.clone()));
        let fn_proto = GcRef::new(JsObject::new(None, memory_manager));
        setup_global_object(global, fn_proto);

        // globalThis should reference the global object itself
        let global_this = global.get(&PropertyKey::string("globalThis"));
        assert!(global_this.is_some());

        // The globalThis value should be an object
        let gt = global_this.unwrap();
        assert!(gt.is_object());
    }

    #[test]
    fn test_is_finite() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        // Finite numbers
        assert_eq!(
            global_is_finite(&Value::undefined(), &[Value::number(42.0)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            global_is_finite(&Value::undefined(), &[Value::number(0.0)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );

        // Non-finite
        assert_eq!(
            global_is_finite(&Value::undefined(), &[Value::number(f64::INFINITY)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            global_is_finite(&Value::undefined(), &[Value::number(f64::NEG_INFINITY)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            global_is_finite(&Value::undefined(), &[Value::number(f64::NAN)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_is_nan() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        assert_eq!(
            global_is_nan(&Value::undefined(), &[Value::number(f64::NAN)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            global_is_nan(&Value::undefined(), &[Value::number(42.0)], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            global_is_nan(&Value::undefined(), &[Value::undefined()], memory_manager.clone())
                .unwrap()
                .as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn test_parse_int() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        // Basic integers
        assert_eq!(
            global_parse_int(
                &Value::undefined(),
                &[Value::string(JsString::intern("42"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(42.0)
        );
        assert_eq!(
            global_parse_int(
                &Value::undefined(),
                &[Value::string(JsString::intern("-123"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(-123.0)
        );
        assert_eq!(
            global_parse_int(
                &Value::undefined(),
                &[Value::string(JsString::intern("+456"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(456.0)
        );

        // With radix
        assert_eq!(
            global_parse_int(
                &Value::undefined(),
                &[Value::string(JsString::intern("ff")), Value::number(16.0)],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(255.0)
        );
        assert_eq!(
            global_parse_int(
                &Value::undefined(),
                &[Value::string(JsString::intern("1010")), Value::number(2.0)],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(10.0)
        );

        // Hex prefix
        assert_eq!(
            global_parse_int(
                &Value::undefined(),
                &[Value::string(JsString::intern("0xFF"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(255.0)
        );

        // Stops at invalid char
        assert_eq!(
            global_parse_int(
                &Value::undefined(),
                &[Value::string(JsString::intern("123abc"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(123.0)
        );

        // Invalid - returns NaN
        let result = global_parse_int(
            &Value::undefined(),
            &[Value::string(JsString::intern("hello"))],
            memory_manager.clone(),
        )
        .unwrap();
        assert!(result.is_nan());
        assert!(result.as_number().unwrap().is_nan());
    }

    #[test]
    fn test_parse_float() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        assert_eq!(
            global_parse_float(
                &Value::undefined(),
                &[Value::string(JsString::intern("3.5"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(3.5)
        );
        assert_eq!(
            global_parse_float(
                &Value::undefined(),
                &[Value::string(JsString::intern("-2.5"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(-2.5)
        );
        assert_eq!(
            global_parse_float(
                &Value::undefined(),
                &[Value::string(JsString::intern("  42  "))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(42.0)
        );
        assert_eq!(
            global_parse_float(
                &Value::undefined(),
                &[Value::string(JsString::intern("Infinity"))],
                memory_manager.clone()
            )
            .unwrap()
            .as_number(),
            Some(f64::INFINITY)
        );
    }

    #[test]
    fn test_encode_uri_component() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let result = global_encode_uri_component(
            &Value::undefined(),
            &[Value::string(JsString::intern("hello world"))],
            memory_manager.clone(),
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");

        let result = global_encode_uri_component(
            &Value::undefined(),
            &[Value::string(JsString::intern("a=1&b=2"))],
            memory_manager.clone(),
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "a%3D1%26b%3D2");
    }

    #[test]
    fn test_decode_uri_component() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let result = global_decode_uri_component(
            &Value::undefined(),
            &[Value::string(JsString::intern("hello%20world"))],
            memory_manager.clone(),
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello world");

        let result = global_decode_uri_component(
            &Value::undefined(),
            &[Value::string(JsString::intern("a%3D1%26b%3D2"))],
            memory_manager.clone(),
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "a=1&b=2");
    }

    #[test]
    fn test_encode_uri() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        // encodeURI does not encode reserved characters
        let result = global_encode_uri(
            &Value::undefined(),
            &[Value::string(JsString::intern(
                "http://example.com/path?q=1",
            ))],
            memory_manager.clone(),
        )
        .unwrap();
        assert_eq!(
            result.as_string().unwrap().as_str(),
            "http://example.com/path?q=1"
        );

        // But does encode other special chars
        let result = global_encode_uri(
            &Value::undefined(),
            &[Value::string(JsString::intern("hello world"))],
            memory_manager.clone(),
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");
    }

    #[test]
    fn test_decode_uri() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        let result = global_decode_uri(
            &Value::undefined(),
            &[Value::string(JsString::intern("hello%20world"))],
            memory_manager.clone(),
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello world");
    }

    #[test]
    fn test_eval_non_string() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        // eval with non-string returns the value unchanged
        assert_eq!(
            global_eval(&Value::undefined(), &[Value::number(42.0)], memory_manager.clone())
                .unwrap()
                .as_number(),
            Some(42.0)
        );
        assert!(global_eval(&Value::undefined(), &[Value::undefined()], memory_manager.clone())
            .unwrap()
            .is_undefined());
    }

    #[test]
    fn test_eval_string() {
        let memory_manager = Arc::new(crate::memory::MemoryManager::test());
        // eval with string is not supported
        let result = global_eval(
            &Value::undefined(),
            &[Value::string(JsString::intern("1 + 1"))],
            memory_manager.clone(),
        );
        assert!(result.is_err());
    }
}
