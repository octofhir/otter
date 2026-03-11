//! Global object setup for JavaScript environment
//!
//! Provides the standard global functions and values:
//! - `globalThis` - reference to the global object itself
//! - `undefined`, `NaN`, `Infinity` - primitive values
//! - `eval`, `isFinite`, `isNaN`, `parseInt`, `parseFloat` - functions
//! - `encodeURI`, `decodeURI`, `encodeURIComponent`, `decodeURIComponent` - URI encoding

use std::sync::Arc;

use num_bigint::BigInt as NumBigInt;
use num_traits::ToPrimitive;

use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;

/// Create a native function with proper `length` and `name` properties,
/// and define it on the target object with builtin_method attributes
/// (`{ writable: true, enumerable: false, configurable: true }`).
fn define_global_fn<F>(
    target: &GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    func: F,
    name: &str,
    length: u32,
) where
    F: Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync
        + 'static,
{
    let fn_obj = GcRef::new(JsObject::new(Value::object(fn_proto)));
    fn_obj.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::function_length(Value::number(length as f64)),
    );
    fn_obj.define_property(
        PropertyKey::string("name"),
        PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
    );
    // Built-in global functions are not constructors (ES2023 §17)
    let _ = fn_obj.set(
        PropertyKey::string("__non_constructor"),
        Value::boolean(true),
    );
    let native_fn: Arc<
        dyn Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
            + Send
            + Sync,
    > = Arc::new(func);
    let value =
        Value::native_function_with_proto_and_object(native_fn, mm.clone(), fn_proto, fn_obj);
    target.define_property(
        PropertyKey::string(name),
        PropertyDescriptor::builtin_method(value),
    );
}

fn get_arg(args: &[Value], index: usize) -> Value {
    args.get(index).cloned().unwrap_or(Value::undefined())
}

/// Set up all standard global properties on the global object.
///
/// `fn_proto` is the intrinsic `%Function.prototype%` created by VmRuntime.
/// All native functions receive it as their `[[Prototype]]` per ES2023 §10.3.1.
pub fn setup_global_object(
    global: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    _intrinsics_opt: Option<&crate::intrinsics::Intrinsics>,
) {
    let mm = crate::memory::MemoryManager::current()
        .expect("MemoryManager must be set before setup_global_object");

    // globalThis - self-referencing, per spec: {writable: true, enumerable: false, configurable: false}
    global.define_property(
        PropertyKey::string("globalThis"),
        PropertyDescriptor::Data {
            value: Value::object(global),
            attributes: PropertyAttributes {
                writable: true,
                enumerable: false,
                configurable: false,
            },
        },
    );

    // Primitive values — per ES2023 §19.1: {writable: false, enumerable: false, configurable: false}
    let immutable_attrs = PropertyAttributes {
        writable: false,
        enumerable: false,
        configurable: false,
    };
    global.define_property(
        PropertyKey::string("undefined"),
        PropertyDescriptor::Data {
            value: Value::undefined(),
            attributes: immutable_attrs,
        },
    );
    global.define_property(
        PropertyKey::string("NaN"),
        PropertyDescriptor::Data {
            value: Value::number(f64::NAN),
            attributes: immutable_attrs,
        },
    );
    global.define_property(
        PropertyKey::string("Infinity"),
        PropertyDescriptor::Data {
            value: Value::number(f64::INFINITY),
            attributes: immutable_attrs,
        },
    );

    // Global functions — all get fn_proto as [[Prototype]] with proper length/name
    // Per spec §19.2, these are { writable: true, enumerable: false, configurable: true }
    define_global_fn(&global, &mm, fn_proto, global_eval, "eval", 1);
    define_global_fn(
        &global,
        &mm,
        fn_proto,
        global_eval_script,
        "__evalScript",
        1,
    );
    define_global_fn(&global, &mm, fn_proto, global_is_finite, "isFinite", 1);
    define_global_fn(&global, &mm, fn_proto, global_is_nan, "isNaN", 1);
    define_global_fn(&global, &mm, fn_proto, global_parse_int, "parseInt", 2);
    define_global_fn(&global, &mm, fn_proto, global_parse_float, "parseFloat", 1);

    // URI encoding/decoding functions
    define_global_fn(&global, &mm, fn_proto, global_encode_uri, "encodeURI", 1);
    define_global_fn(&global, &mm, fn_proto, global_decode_uri, "decodeURI", 1);
    define_global_fn(
        &global,
        &mm,
        fn_proto,
        global_encode_uri_component,
        "encodeURIComponent",
        1,
    );
    define_global_fn(
        &global,
        &mm,
        fn_proto,
        global_decode_uri_component,
        "decodeURIComponent",
        1,
    );

    // Annex B legacy functions
    define_global_fn(&global, &mm, fn_proto, global_escape, "escape", 1);
    define_global_fn(&global, &mm, fn_proto, global_unescape, "unescape", 1);
}

/// `eval(x)` - Evaluates JavaScript code represented as a string.
///
/// Currently, indirect eval is not fully supported. When called with a string,
/// it returns an error. This is a limitation to be addressed in a future update.
/// `__evalScript(code)` - Compile and execute code as a global script.
/// For $262.evalScript semantics: top-level `let`/`const` behave as global bindings.
fn global_eval_script(
    _this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let arg = get_arg(args, 0);
    if !arg.is_string() {
        return Ok(arg);
    }
    let source = arg
        .as_string()
        .ok_or_else(|| VmError::type_error("evalScript argument is not a string"))?;
    ncx.eval_as_global_script(source.as_str())
}

fn global_eval(
    this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    // Per spec: if argument is not a string, return it unchanged
    let arg = get_arg(args, 0);

    if arg.is_string() {
        let source = arg
            .as_string()
            .ok_or_else(|| VmError::type_error("eval argument is not a string"))?;
        let module = ncx.ctx.compile_eval(source.as_str(), false)?;
        let realm_id = this
            .as_object()
            .and_then(|obj| obj.get(&PropertyKey::string("__realm_id__")))
            .and_then(|v| v.as_int32())
            .map(|id| id as u32);
        if let Some(realm_id) = realm_id {
            ncx.execute_eval_module_in_realm(realm_id, &module)
        } else {
            ncx.execute_eval_module(&module)
        }
    } else {
        // Non-string argument: return it unchanged (per spec §19.2.1.1)
        Ok(arg)
    }
}

/// `isFinite(number)` - Determines whether the passed value is a finite number.
/// Per §19.2.2, calls ToNumber which invokes ToPrimitive on objects.
fn global_is_finite(
    _this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let value = get_arg(args, 0);
    let num = ncx.to_number_value(&value)?;
    Ok(Value::boolean(num.is_finite()))
}

/// `isNaN(number)` - Determines whether a value is NaN.
/// Per §19.2.3, calls ToNumber which invokes ToPrimitive on objects.
fn global_is_nan(
    _this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let value = get_arg(args, 0);
    let num = ncx.to_number_value(&value)?;
    Ok(Value::boolean(num.is_nan()))
}

/// `parseInt(string, radix)` - Parses a string and returns an integer.
fn global_parse_int(
    _this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let radix_arg = args.get(1);

    // Convert input to string via JS ToString (handles objects, BigInt, etc.)
    let input_str = ncx.to_string_value(&input)?;
    let trimmed = input_str.trim();

    if trimmed.is_empty() {
        return Ok(Value::number(f64::NAN));
    }

    // Determine sign
    let (sign, rest) = if let Some(s) = trimmed.strip_prefix('-') {
        (-1.0f64, s)
    } else if let Some(s) = trimmed.strip_prefix('+') {
        (1.0f64, s)
    } else {
        (1.0f64, trimmed)
    };

    // Determine radix via ToInt32 (ES spec 7.1.6)
    let mut radix: u32 = match radix_arg {
        Some(r) => {
            let n = ncx.to_number_value(r)?;
            let n_i32 = to_int32(n);
            if n_i32 == 0 {
                10 // default
            } else if !(2..=36).contains(&n_i32) {
                return Ok(Value::number(f64::NAN));
            } else {
                n_i32 as u32
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

    // Parse digits one by one until we hit an invalid character.
    // Use f64 to handle arbitrarily large integers (matching JS behavior).
    let mut result: f64 = 0.0;
    let mut any_valid = false;

    for c in digits.chars() {
        let digit = match c.to_digit(radix) {
            Some(d) => d as f64,
            None => break, // Stop at first invalid character
        };
        any_valid = true;
        result = result * (radix as f64) + digit;
    }

    if !any_valid {
        return Ok(Value::number(f64::NAN));
    }

    Ok(Value::number(sign * result))
}

/// `parseFloat(string)` - Parses a string and returns a floating point number.
fn global_parse_float(
    _this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let input_str = ncx.to_string_value(&input)?;
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
    _ncx: &mut crate::context::NativeContext<'_>,
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
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let encoded = ncx.to_string_value(&input)?;

    decode_uri_impl(&encoded, true)
}

/// `encodeURIComponent(str)` - Encodes a URI component.
fn global_encode_uri_component(
    _this: &Value,
    args: &[Value],
    _ncx: &mut crate::context::NativeContext<'_>,
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
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let encoded = ncx.to_string_value(&input)?;

    decode_uri_impl(&encoded, false)
}

/// Read a single %XX hex escape from bytes. Returns the byte value.
fn read_percent_hex(bytes: &[u8], pos: usize) -> Result<u8, VmError> {
    if pos + 2 >= bytes.len() {
        return Err(VmError::uri_error("URI malformed"));
    }
    if bytes[pos] != b'%' {
        return Err(VmError::uri_error("URI malformed"));
    }
    let h1 = bytes[pos + 1];
    let h2 = bytes[pos + 2];
    if !h1.is_ascii_hexdigit() || !h2.is_ascii_hexdigit() {
        return Err(VmError::uri_error("URI malformed"));
    }
    let val = hex_val(h1) * 16 + hex_val(h2);
    Ok(val)
}

fn hex_val(b: u8) -> u8 {
    match b {
        b'0'..=b'9' => b - b'0',
        b'a'..=b'f' => b - b'a' + 10,
        b'A'..=b'F' => b - b'A' + 10,
        _ => 0,
    }
}

/// Common implementation for decodeURI and decodeURIComponent (ES2026 §19.2.6.4)
fn decode_uri_impl(encoded: &str, preserve_reserved: bool) -> Result<Value, VmError> {
    let bytes = encoded.as_bytes();
    let len = bytes.len();
    let mut result = Vec::with_capacity(len);
    let mut k = 0;

    while k < len {
        if bytes[k] == b'%' {
            // Read first byte
            let b = read_percent_hex(bytes, k)?;
            k += 3;

            if b & 0x80 == 0 {
                // Single-byte ASCII character
                let c = b as char;
                if preserve_reserved && (URI_RESERVED.contains(c) || c == '#') {
                    // Keep encoded form
                    result.push(b'%');
                    result.push(bytes[k - 2]);
                    result.push(bytes[k - 1]);
                } else {
                    result.push(b);
                }
            } else {
                // Multi-byte UTF-8 sequence
                let n = if b & 0xE0 == 0xC0 {
                    2
                } else if b & 0xF0 == 0xE0 {
                    3
                } else if b & 0xF8 == 0xF0 {
                    4
                } else {
                    return Err(VmError::uri_error("URI malformed"));
                };

                let mut utf8_bytes = Vec::with_capacity(n);
                utf8_bytes.push(b);

                for _j in 1..n {
                    if k >= len || bytes[k] != b'%' {
                        return Err(VmError::uri_error("URI malformed"));
                    }
                    let cont = read_percent_hex(bytes, k)?;
                    k += 3;
                    // Validate continuation byte: must be 10xxxxxx
                    if cont & 0xC0 != 0x80 {
                        return Err(VmError::uri_error("URI malformed"));
                    }
                    utf8_bytes.push(cont);
                }

                // Decode UTF-8 to a code point
                let cp = match n {
                    2 => {
                        let cp =
                            ((utf8_bytes[0] as u32 & 0x1F) << 6) | (utf8_bytes[1] as u32 & 0x3F);
                        // Overlong check: must be >= 0x80
                        if cp < 0x80 {
                            return Err(VmError::uri_error("URI malformed"));
                        }
                        cp
                    }
                    3 => {
                        let cp = ((utf8_bytes[0] as u32 & 0x0F) << 12)
                            | ((utf8_bytes[1] as u32 & 0x3F) << 6)
                            | (utf8_bytes[2] as u32 & 0x3F);
                        // Overlong check: must be >= 0x800
                        if cp < 0x800 {
                            return Err(VmError::uri_error("URI malformed"));
                        }
                        // Reject surrogates U+D800-U+DFFF
                        if (0xD800..=0xDFFF).contains(&cp) {
                            return Err(VmError::uri_error("URI malformed"));
                        }
                        cp
                    }
                    4 => {
                        let cp = ((utf8_bytes[0] as u32 & 0x07) << 18)
                            | ((utf8_bytes[1] as u32 & 0x3F) << 12)
                            | ((utf8_bytes[2] as u32 & 0x3F) << 6)
                            | (utf8_bytes[3] as u32 & 0x3F);
                        // Overlong check: must be >= 0x10000
                        if cp < 0x10000 {
                            return Err(VmError::uri_error("URI malformed"));
                        }
                        // Must be valid Unicode (max U+10FFFF)
                        if cp > 0x10FFFF {
                            return Err(VmError::uri_error("URI malformed"));
                        }
                        cp
                    }
                    _ => return Err(VmError::uri_error("URI malformed")),
                };

                // For decodeURI, check if the decoded character is reserved
                if preserve_reserved
                    && cp < 128
                    && (URI_RESERVED.contains(cp as u8 as char) || cp as u8 as char == '#')
                {
                    // Keep encoded form of all bytes
                    for byte in &utf8_bytes {
                        result.push(b'%');
                        result.push(b"0123456789ABCDEF"[(*byte >> 4) as usize]);
                        result.push(b"0123456789ABCDEF"[(*byte & 0x0F) as usize]);
                    }
                } else {
                    // Append UTF-8 bytes
                    result.extend_from_slice(&utf8_bytes);
                }
            }
        } else {
            result.push(bytes[k]);
            k += 1;
        }
    }

    // Convert bytes to string
    let decoded = String::from_utf8(result).map_err(|_| VmError::uri_error("URI malformed"))?;

    Ok(Value::string(JsString::intern(&decoded)))
}

// =============================================================================
// Annex B: escape / unescape (§B.2.1, §B.2.2)
// =============================================================================

/// `escape(string)` — Annex B §B.2.1
/// Encodes a string, replacing all characters except `A-Z a-z 0-9 @ * _ + - . /`
/// with `%XX` or `%uXXXX` escape sequences.
fn global_escape(
    _this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = ncx.to_string_value(&get_arg(args, 0))?;
    let mut result = String::with_capacity(input.len());
    for ch in input.encode_utf16() {
        let c = ch;
        // Characters that are NOT escaped
        if matches!(c, 0x41..=0x5A | 0x61..=0x7A | 0x30..=0x39) // A-Z, a-z, 0-9
            || matches!(c, 0x40 | 0x2A | 0x5F | 0x2B | 0x2D | 0x2E | 0x2F)
        // @ * _ + - . /
        {
            result.push(char::from(c as u8));
        } else if c < 256 {
            result.push_str(&format!("%{:02X}", c));
        } else {
            result.push_str(&format!("%u{:04X}", c));
        }
    }
    Ok(Value::string(JsString::intern(&result)))
}

/// `unescape(string)` — Annex B §B.2.2
/// Decodes a string produced by `escape()`.
fn global_unescape(
    _this: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = ncx.to_string_value(&get_arg(args, 0))?;
    // Work with UTF-16 code units per spec, then convert back
    let units: Vec<u16> = input.encode_utf16().collect();
    let len = units.len();
    let mut result_units: Vec<u16> = Vec::with_capacity(len);
    let mut i = 0;
    while i < len {
        if units[i] == b'%' as u16 {
            // Try %uXXXX first (6 code units total)
            if i + 5 < len && units[i + 1] == b'u' as u16 {
                if let Some(code) = parse_hex4_u16(&units[i + 2..i + 6]) {
                    result_units.push(code);
                    i += 6;
                    continue;
                }
            }
            // Try %XX (3 code units total)
            if i + 2 < len {
                if let Some(code) = parse_hex2_u16(&units[i + 1..i + 3]) {
                    result_units.push(code);
                    i += 3;
                    continue;
                }
            }
        }
        result_units.push(units[i]);
        i += 1;
    }
    let decoded = String::from_utf16_lossy(&result_units);
    Ok(Value::string(JsString::intern(&decoded)))
}

fn parse_hex2(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 2 {
        return None;
    }
    let high = hex_digit(bytes[0])?;
    let low = hex_digit(bytes[1])?;
    Some((high as u16) * 16 + low as u16)
}

fn parse_hex4(bytes: &[u8]) -> Option<u16> {
    if bytes.len() < 4 {
        return None;
    }
    let a = hex_digit(bytes[0])? as u16;
    let b = hex_digit(bytes[1])? as u16;
    let c = hex_digit(bytes[2])? as u16;
    let d = hex_digit(bytes[3])? as u16;
    Some(a * 4096 + b * 256 + c * 16 + d)
}

fn hex_digit(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'A'..=b'F' => Some(b - b'A' + 10),
        b'a'..=b'f' => Some(b - b'a' + 10),
        _ => None,
    }
}

/// Parse two hex digit code units (u16) into a byte value
fn parse_hex2_u16(units: &[u16]) -> Option<u16> {
    if units.len() < 2 {
        return None;
    }
    let high = hex_digit_u16(units[0])?;
    let low = hex_digit_u16(units[1])?;
    Some((high as u16) * 16 + low as u16)
}

/// Parse four hex digit code units (u16) into a u16 value
fn parse_hex4_u16(units: &[u16]) -> Option<u16> {
    if units.len() < 4 {
        return None;
    }
    let a = hex_digit_u16(units[0])? as u16;
    let b = hex_digit_u16(units[1])? as u16;
    let c = hex_digit_u16(units[2])? as u16;
    let d = hex_digit_u16(units[3])? as u16;
    Some(a * 4096 + b * 256 + c * 16 + d)
}

fn hex_digit_u16(u: u16) -> Option<u8> {
    match u {
        0x30..=0x39 => Some((u - 0x30) as u8),      // '0'-'9'
        0x41..=0x46 => Some((u - 0x41) as u8 + 10), // 'A'-'F'
        0x61..=0x66 => Some((u - 0x61) as u8 + 10), // 'a'-'f'
        _ => None,
    }
}

// =============================================================================
// Type conversion helpers
// =============================================================================

/// Convert a Value to a number (ToNumber abstract operation)
/// ES2023 ToInt32 abstract operation (7.1.6).
pub fn to_int32(n: f64) -> i32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    let i = n.trunc() as i64;
    (i % (1_i64 << 32)) as i32
}

pub fn to_number(value: &Value) -> f64 {
    if let Some(n) = value.as_number() {
        return n;
    }
    if let Some(b) = value.as_bigint() {
        let mut s = b.value.as_str();
        let negative = s.starts_with('-');
        if negative {
            s = &s[1..];
        }
        if let Some(mut bigint) = NumBigInt::parse_bytes(s.as_bytes(), 10) {
            if negative {
                bigint = -bigint;
            }
            return bigint.to_f64().unwrap_or(if negative {
                f64::NEG_INFINITY
            } else {
                f64::INFINITY
            });
        }
        return f64::NAN;
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
    } else if let Some(obj) = value.as_object() {
        use crate::object::PropertyKey;
        if let Some(prim) = obj.get(&PropertyKey::string("__value__")) {
            return to_number(&prim);
        }
        if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
            return to_number(&prim);
        }
        f64::NAN
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

/// ES2023 Number::toString(10) — convert f64 to JS string representation.
///
/// Rules:
/// - NaN → "NaN", ±Infinity → "Infinity"/"-Infinity"
/// - Integers with |n| < 10^21 → no decimal point, no exponent
/// - Otherwise → shortest representation (scientific notation for large/small)
pub fn js_number_to_string(n: f64) -> String {
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
    if n == 0.0 {
        return "0".to_string();
    }

    let negative = n < 0.0 || (n == 0.0 && n.is_sign_negative());
    let abs_n = n.abs();

    // Integer fast path: use itoa for zero-alloc integer formatting.
    // Only use `as u64` for values within safe integer range (2^53) where
    // the f64→u64 conversion is exact. Larger values go through ryu to get
    // the shortest representation that round-trips correctly.
    if abs_n.fract() == 0.0 && abs_n < 1e21 {
        if abs_n <= 9007199254740992.0 {
            // Safe integer range (≤ 2^53): exact conversion
            let int_val = abs_n as u64;
            let mut buf = itoa::Buffer::new();
            let s = buf.format(int_val);
            return if negative {
                let mut result = String::with_capacity(s.len() + 1);
                result.push('-');
                result.push_str(s);
                result
            } else {
                s.to_string()
            };
        }
        // Large integers (> 2^53 but < 1e21): fall through to ryu path
        // for shortest representation
    }

    // Float path: use ryu for shortest representation, then apply JS rules.
    // ryu::Buffer::format_finite() returns the shortest decimal &str.
    let mut ryu_buf = ryu::Buffer::new();
    let repr = ryu_buf.format_finite(abs_n);

    // Parse mantissa digits and exponent from ryu output.
    // ryu format: "1.23E5", "1.5", "0.001", "100.0", etc.
    let (digits, n) = parse_ryu_output(repr);
    let k = digits.len() as i32;

    // Apply ECMA-262 §7.1.12.1 Number::toString formatting rules.
    let result = format_js_number_parts(&digits, k, n);

    if negative {
        let mut s = String::with_capacity(result.len() + 1);
        s.push('-');
        s.push_str(&result);
        s
    } else {
        result
    }
}

/// Parse ryu output into (significant_digits, n) where n = exponent+1
/// per ECMA-262 §7.1.12.1 (s × 10^(n−k) = |value|, k minimal).
///
/// ryu format examples: "1.5" → ("15", 1), "1.23E5" → ("123", 6),
/// "0.001" → ("1", -2), "100.0" → ("1", 3)
fn parse_ryu_output(repr: &str) -> (String, i32) {
    // Scientific format: "1.23E5" or "1E-20"
    let e_pos = repr.find('E').or_else(|| repr.find('e'));
    if let Some(pos) = e_pos {
        let mantissa_part = &repr[..pos];
        let exp: i32 = repr[pos + 1..].parse().unwrap_or(0);

        // Extract digits from mantissa (skip dot and minus)
        let mut digits = String::with_capacity(mantissa_part.len());
        for c in mantissa_part.chars() {
            if c.is_ascii_digit() {
                digits.push(c);
            }
        }
        // Strip trailing zeros
        while digits.len() > 1 && digits.ends_with('0') {
            digits.pop();
        }
        // n = exp + 1 (mantissa is D.DDD form, so MSD is at 10^exp)
        return (digits, exp + 1);
    }

    // Plain format: "1.5", "0.1", "100.0", "0.001"
    if let Some(dot_pos) = repr.find('.') {
        let before_dot = &repr[..dot_pos];
        let after_dot = &repr[dot_pos + 1..];

        if before_dot == "0" || before_dot == "-0" {
            // "0.xxx" format: significant digits start after leading zeros
            let leading_zeros = after_dot.chars().take_while(|c| *c == '0').count();
            let mut digits: String = after_dot[leading_zeros..].to_string();
            // Strip trailing zeros
            while digits.len() > 1 && digits.ends_with('0') {
                digits.pop();
            }
            if digits.is_empty() {
                digits.push('0');
            }
            let n = -(leading_zeros as i32);
            return (digits, n);
        }

        // "DDD.DDD" format (e.g. "1.5", "100.0")
        let mut all_digits = String::with_capacity(before_dot.len() + after_dot.len());
        all_digits.push_str(before_dot);
        all_digits.push_str(after_dot);
        // Strip trailing zeros
        while all_digits.len() > 1 && all_digits.ends_with('0') {
            all_digits.pop();
        }
        let n = before_dot.len() as i32;
        return (all_digits, n);
    }

    // No dot, no exponent (shouldn't happen with ryu for valid f64)
    let n = repr.len() as i32;
    (repr.to_string(), n)
}

/// Format number parts per ECMA-262 §7.1.12.1 rules.
fn format_js_number_parts(digits: &str, k: i32, n: i32) -> String {
    if k <= n && n <= 21 {
        // Case: integer-like, append zeros
        let mut s = String::with_capacity(n as usize);
        s.push_str(digits);
        for _ in 0..(n - k) {
            s.push('0');
        }
        s
    } else if 0 < n && n <= k {
        // Case: decimal point within the digits
        let n_usize = n as usize;
        let mut s = String::with_capacity(k as usize + 1);
        s.push_str(&digits[..n_usize]);
        s.push('.');
        s.push_str(&digits[n_usize..]);
        s
    } else if -6 < n && n <= 0 {
        // Case: "0.000...digits"
        let zeros = (-n) as usize;
        let mut s = String::with_capacity(2 + zeros + digits.len());
        s.push_str("0.");
        for _ in 0..zeros {
            s.push('0');
        }
        s.push_str(digits);
        s
    } else {
        // Scientific notation
        let exp_val = n - 1;
        let sign_char = if exp_val >= 0 { "+" } else { "" };
        if k == 1 {
            let mut s = String::with_capacity(6);
            s.push_str(&digits[..1]);
            s.push('e');
            s.push_str(sign_char);
            // Use itoa for exponent
            let mut ebuf = itoa::Buffer::new();
            s.push_str(ebuf.format(exp_val));
            s
        } else {
            let mut s = String::with_capacity(k as usize + 6);
            s.push_str(&digits[..1]);
            s.push('.');
            s.push_str(&digits[1..]);
            s.push('e');
            s.push_str(sign_char);
            let mut ebuf = itoa::Buffer::new();
            s.push_str(ebuf.format(exp_val));
            s
        }
    }
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
        return js_number_to_string(n);
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

    type GlobalFn =
        fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>;

    fn call_global(
        runtime: &crate::runtime::VmRuntime,
        fn_impl: GlobalFn,
        args: &[Value],
    ) -> Result<Value, VmError> {
        let mut ctx = runtime.create_context();
        let interpreter = crate::interpreter::Interpreter::new();
        let mut ncx = crate::context::NativeContext::new(&mut ctx, &interpreter);
        fn_impl(&Value::undefined(), args, &mut ncx)
    }

    #[test]
    fn test_global_this_setup() {
        let runtime = crate::runtime::VmRuntime::new();
        let memory_manager = runtime.memory_manager().clone();
        let global = GcRef::new(JsObject::new(Value::null()));
        let fn_proto = GcRef::new(JsObject::new(Value::null()));
        setup_global_object(global, fn_proto, None);

        // globalThis should reference the global object itself
        let global_this = global.get(&PropertyKey::string("globalThis"));
        assert!(global_this.is_some());

        // The globalThis value should be an object
        let gt = global_this.unwrap();
        assert!(gt.is_object());
    }

    #[test]
    fn test_is_finite() {
        let _rt = crate::runtime::VmRuntime::new();
        // Finite numbers
        assert_eq!(
            call_global(&_rt, global_is_finite, &[Value::number(42.0)])
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            call_global(&_rt, global_is_finite, &[Value::number(0.0)])
                .unwrap()
                .as_boolean(),
            Some(true)
        );

        // Non-finite
        assert_eq!(
            call_global(&_rt, global_is_finite, &[Value::number(f64::INFINITY)])
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            call_global(&_rt, global_is_finite, &[Value::number(f64::NEG_INFINITY)])
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            call_global(&_rt, global_is_finite, &[Value::number(f64::NAN)])
                .unwrap()
                .as_boolean(),
            Some(false)
        );
    }

    #[test]
    fn test_is_nan() {
        let _rt = crate::runtime::VmRuntime::new();
        assert_eq!(
            call_global(&_rt, global_is_nan, &[Value::number(f64::NAN)])
                .unwrap()
                .as_boolean(),
            Some(true)
        );
        assert_eq!(
            call_global(&_rt, global_is_nan, &[Value::number(42.0)])
                .unwrap()
                .as_boolean(),
            Some(false)
        );
        assert_eq!(
            call_global(&_rt, global_is_nan, &[Value::undefined()])
                .unwrap()
                .as_boolean(),
            Some(true)
        );
    }

    #[test]
    fn test_parse_int() {
        let _rt = crate::runtime::VmRuntime::new();
        // Basic integers
        assert_eq!(
            call_global(
                &_rt,
                global_parse_int,
                &[Value::string(JsString::intern("42"))]
            )
            .unwrap()
            .as_number(),
            Some(42.0)
        );
        assert_eq!(
            call_global(
                &_rt,
                global_parse_int,
                &[Value::string(JsString::intern("-123"))]
            )
            .unwrap()
            .as_number(),
            Some(-123.0)
        );
        assert_eq!(
            call_global(
                &_rt,
                global_parse_int,
                &[Value::string(JsString::intern("+456"))]
            )
            .unwrap()
            .as_number(),
            Some(456.0)
        );

        // With radix
        assert_eq!(
            call_global(
                &_rt,
                global_parse_int,
                &[Value::string(JsString::intern("ff")), Value::number(16.0)],
            )
            .unwrap()
            .as_number(),
            Some(255.0)
        );
        assert_eq!(
            call_global(
                &_rt,
                global_parse_int,
                &[Value::string(JsString::intern("1010")), Value::number(2.0)],
            )
            .unwrap()
            .as_number(),
            Some(10.0)
        );

        // Hex prefix
        assert_eq!(
            call_global(
                &_rt,
                global_parse_int,
                &[Value::string(JsString::intern("0xFF"))]
            )
            .unwrap()
            .as_number(),
            Some(255.0)
        );

        // Stops at invalid char
        assert_eq!(
            call_global(
                &_rt,
                global_parse_int,
                &[Value::string(JsString::intern("123abc"))]
            )
            .unwrap()
            .as_number(),
            Some(123.0)
        );

        // Invalid - returns NaN
        let result = call_global(
            &_rt,
            global_parse_int,
            &[Value::string(JsString::intern("hello"))],
        )
        .unwrap();
        assert!(result.is_nan());
        assert!(result.as_number().unwrap().is_nan());
    }

    #[test]
    fn test_parse_float() {
        let _rt = crate::runtime::VmRuntime::new();
        assert_eq!(
            call_global(
                &_rt,
                global_parse_float,
                &[Value::string(JsString::intern("3.5"))]
            )
            .unwrap()
            .as_number(),
            Some(3.5)
        );
        assert_eq!(
            call_global(
                &_rt,
                global_parse_float,
                &[Value::string(JsString::intern("-2.5"))]
            )
            .unwrap()
            .as_number(),
            Some(-2.5)
        );
        assert_eq!(
            call_global(
                &_rt,
                global_parse_float,
                &[Value::string(JsString::intern("  42  "))]
            )
            .unwrap()
            .as_number(),
            Some(42.0)
        );
        assert_eq!(
            call_global(
                &_rt,
                global_parse_float,
                &[Value::string(JsString::intern("Infinity"))]
            )
            .unwrap()
            .as_number(),
            Some(f64::INFINITY)
        );
    }

    #[test]
    fn test_encode_uri_component() {
        let _rt = crate::runtime::VmRuntime::new();
        let result = call_global(
            &_rt,
            global_encode_uri_component,
            &[Value::string(JsString::intern("hello world"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");

        let result = call_global(
            &_rt,
            global_encode_uri_component,
            &[Value::string(JsString::intern("a=1&b=2"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "a%3D1%26b%3D2");
    }

    #[test]
    fn test_decode_uri_component() {
        let _rt = crate::runtime::VmRuntime::new();
        let result = call_global(
            &_rt,
            global_decode_uri_component,
            &[Value::string(JsString::intern("hello%20world"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello world");

        let result = call_global(
            &_rt,
            global_decode_uri_component,
            &[Value::string(JsString::intern("a%3D1%26b%3D2"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "a=1&b=2");
    }

    #[test]
    fn test_encode_uri() {
        let _rt = crate::runtime::VmRuntime::new();
        // encodeURI does not encode reserved characters
        let result = call_global(
            &_rt,
            global_encode_uri,
            &[Value::string(JsString::intern(
                "http://example.com/path?q=1",
            ))],
        )
        .unwrap();
        assert_eq!(
            result.as_string().unwrap().as_str(),
            "http://example.com/path?q=1"
        );

        // But does encode other special chars
        let result = call_global(
            &_rt,
            global_encode_uri,
            &[Value::string(JsString::intern("hello world"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");
    }

    #[test]
    fn test_decode_uri() {
        let _rt = crate::runtime::VmRuntime::new();
        let result = call_global(
            &_rt,
            global_decode_uri,
            &[Value::string(JsString::intern("hello%20world"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello world");
    }

    #[test]
    fn test_eval_non_string() {
        let _rt = crate::runtime::VmRuntime::new();
        // eval with non-string returns the value unchanged
        assert_eq!(
            call_global(&_rt, global_eval, &[Value::number(42.0)])
                .unwrap()
                .as_number(),
            Some(42.0)
        );
        assert!(
            call_global(&_rt, global_eval, &[Value::undefined()])
                .unwrap()
                .is_undefined()
        );
    }

    #[test]
    fn test_eval_string() {
        let _rt = crate::runtime::VmRuntime::new();
        // eval with string is not supported
        let result = call_global(
            &_rt,
            global_eval,
            &[Value::string(JsString::intern("1 + 1"))],
        );
        assert!(result.is_err());
    }
}
