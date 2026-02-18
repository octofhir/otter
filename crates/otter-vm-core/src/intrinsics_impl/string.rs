//! String.prototype methods implementation
//!
//! All String object methods for ES2026 standard.

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::sync::Arc;

/// thisStringValue(value) per ES2023 §22.1.3
///
/// Extracts the string value from `this`:
/// - If `this` is a string primitive, returns it directly.
/// - If `this` is a String wrapper object (`new String("...")`), reads its
///   `[[PrimitiveValue]]` internal slot (stored as `toString()` result or via
///   a `__primitiveValue__` property).
/// - If `this` is any other object, calls `toString()` on it as a fallback.
/// - Otherwise, returns an error (null/undefined).
fn this_string_value(this_val: &Value) -> Result<GcRef<JsString>, String> {
    // Fast path: string primitive
    if let Some(s) = this_val.as_string() {
        return Ok(s);
    }
    // Object path: String wrapper or generic object
    if let Some(obj) = this_val.as_object() {
        // Check for __primitiveValue__ (internal slot for String wrapper objects)
        if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
            if let Some(s) = prim.as_string() {
                return Ok(s);
            }
        }
        // Fallback: try valueOf then toString
        if let Some(val) = obj.get(&PropertyKey::string("valueOf")) {
            if let Some(s) = val.as_string() {
                return Ok(s);
            }
        }
        if let Some(val) = obj.get(&PropertyKey::string("toString")) {
            if let Some(s) = val.as_string() {
                return Ok(s);
            }
        }
        // Last resort: coerce via number-like or use "[object Object]"
        return Ok(JsString::intern("[object Object]"));
    }
    // Number/boolean coercion
    if let Some(n) = this_val.as_number() {
        return Ok(JsString::intern(&crate::globals::js_number_to_string(n)));
    }
    if let Some(b) = this_val.as_boolean() {
        return Ok(JsString::intern(if b { "true" } else { "false" }));
    }
    Err("String.prototype method called on null or undefined".to_string())
}

/// RequireObjectCoercible(this) + ToString(this) per ES2023 §22.1.3
///
/// Most String.prototype methods do:
/// 1. Let O be ? RequireObjectCoercible(this value).
/// 2. Let S be ? ToString(O).
fn require_object_coercible_to_string(
    this_val: &Value,
    ncx: &mut NativeContext,
) -> Result<GcRef<JsString>, VmError> {
    // 1. RequireObjectCoercible: throw TypeError for null/undefined
    if this_val.is_null() || this_val.is_undefined() {
        return Err(VmError::type_error(
            "String.prototype method called on null or undefined",
        ));
    }
    // 2. Fast path: string primitive
    if let Some(s) = this_val.as_string() {
        return Ok(s);
    }
    // 3. String wrapper object fast path
    if let Some(obj) = this_val.as_object() {
        if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
            if let Some(s) = prim.as_string() {
                return Ok(s);
            }
        }
    }
    // 4. ToString via NativeContext (handles numbers, booleans, objects with toString/valueOf)
    let str_result = ncx.to_string_value(this_val)?;
    Ok(JsString::intern(&str_result))
}

/// ToIntegerOrInfinity per ES2023 §7.1.5
fn to_integer_or_infinity(val: &Value, ncx: &mut NativeContext) -> Result<f64, VmError> {
    if val.is_undefined() {
        return Ok(0.0);
    }
    let n = if let Some(n) = val.as_number() {
        n
    } else if let Some(i) = val.as_int32() {
        return Ok(i as f64);
    } else {
        ncx.to_number_value(val)?
    };
    if n.is_nan() || n == 0.0 {
        Ok(0.0)
    } else if n.is_infinite() {
        Ok(n)
    } else {
        Ok(n.trunc())
    }
}

/// Coerce argument to string via ToString, with fast path for string primitives
fn arg_to_string(val: &Value, ncx: &mut NativeContext) -> Result<String, VmError> {
    if val.is_undefined() {
        return Ok("undefined".to_string());
    }
    if let Some(s) = val.as_string() {
        return Ok(s.as_str().to_string());
    }
    ncx.to_string_value(val)
}

/// ES spec whitespace check - includes all characters from WhiteSpace and LineTerminator
/// Rust's char::is_whitespace doesn't include BOM (U+FEFF)
fn is_es_whitespace(c: char) -> bool {
    matches!(c,
        '\u{0009}' | // TAB
        '\u{000B}' | // VT
        '\u{000C}' | // FF
        '\u{0020}' | // SP
        '\u{00A0}' | // NBSP
        '\u{FEFF}' | // BOM / ZWNBSP
        '\u{1680}' | // OGHAM SPACE MARK
        '\u{2000}'..='\u{200A}' | // EN QUAD..HAIR SPACE
        '\u{2028}' | // LINE SEPARATOR
        '\u{2029}' | // PARAGRAPH SEPARATOR
        '\u{202F}' | // NARROW NO-BREAK SPACE
        '\u{205F}' | // MEDIUM MATHEMATICAL SPACE
        '\u{3000}' | // IDEOGRAPHIC SPACE
        '\u{000A}' | // LF
        '\u{000D}'   // CR
    )
}

fn es_trim(s: &str) -> &str {
    let start = s.find(|c: char| !is_es_whitespace(c)).unwrap_or(s.len());
    let end = s.rfind(|c: char| !is_es_whitespace(c)).map(|i| i + s[i..].chars().next().unwrap().len_utf8()).unwrap_or(0);
    if start >= end { "" } else { &s[start..end] }
}

fn es_trim_start(s: &str) -> &str {
    let start = s.find(|c: char| !is_es_whitespace(c)).unwrap_or(s.len());
    &s[start..]
}

fn es_trim_end(s: &str) -> &str {
    let end = s.rfind(|c: char| !is_es_whitespace(c)).map(|i| i + s[i..].chars().next().unwrap().len_utf8()).unwrap_or(0);
    &s[..end]
}

/// Create an array with proper Array.prototype
fn create_array(ncx: &mut NativeContext, length: usize) -> GcRef<JsObject> {
    let arr = GcRef::new(JsObject::array(length, ncx.memory_manager().clone()));
    // Set Array.prototype as prototype
    if let Some(array_ctor) = ncx.ctx.get_global("Array").and_then(|v| v.as_object()) {
        if let Some(proto) = array_ctor.get(&PropertyKey::string("prototype")) {
            arr.set_prototype(proto);
        }
    }
    arr
}

/// Get a property from a Value, properly invoking accessor getters with the correct receiver.
/// Unlike `get_value_full`, this passes the original Value (not obj wrapper) as `this` to getters,
/// which is important for RegExp objects where getters check `this.as_regex()`.
fn get_property_of(
    val: &Value,
    key: &PropertyKey,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let obj = val.as_regex().map(|r| r.object.clone())
        .or_else(|| val.as_object());
    if let Some(obj) = obj {
        if let Some(desc) = obj.lookup_property_descriptor(key) {
            match desc {
                PropertyDescriptor::Data { value, .. } => Ok(value),
                PropertyDescriptor::Accessor { get, .. } => {
                    if let Some(getter) = get {
                        if !getter.is_undefined() {
                            return ncx.call_function(&getter, val.clone(), &[]);
                        }
                    }
                    Ok(Value::undefined())
                }
                PropertyDescriptor::Deleted => Ok(Value::undefined()),
            }
        } else {
            Ok(Value::undefined())
        }
    } else {
        Ok(Value::undefined())
    }
}

/// ES spec IsRegExp (§7.2.8)
/// ALWAYS calls Get(argument, @@match) first, even for RegExp objects.
/// This ensures that custom Symbol.match getters that throw are properly propagated.
fn is_regexp_check(val: &Value, ncx: &mut NativeContext<'_>) -> Result<bool, VmError> {
    if val.is_null() || val.is_undefined() {
        return Ok(false);
    }
    // Step 1: If Type(argument) is not Object, return false
    let has_obj = val.as_regex().is_some() || val.as_object().is_some();
    if !has_obj {
        return Ok(false);
    }
    // Step 2: Let matcher be ? Get(argument, @@match)
    let match_key = PropertyKey::Symbol(crate::intrinsics::well_known::match_symbol());
    let matcher = get_property_of(val, &match_key, ncx)?;
    // Step 3: If matcher is not undefined, return ToBoolean(matcher)
    if !matcher.is_undefined() {
        return Ok(matcher.to_boolean());
    }
    // Step 4: If argument has a [[RegExpMatcher]] internal slot, return true
    if val.as_regex().is_some() {
        return Ok(true);
    }
    // Step 5: Return false
    Ok(false)
}

/// ES spec GetSubstitution (§22.1.3.19.1)
/// Applies replacement patterns: $$ → $, $& → match, $` → before match, $' → after match
fn apply_replacement_pattern(
    replacement: &str,
    matched: &str,
    string: &str,
    match_pos: usize,
) -> String {
    let mut result = String::with_capacity(replacement.len());
    let bytes = replacement.as_bytes();
    let len = bytes.len();
    let mut i = 0;
    while i < len {
        if bytes[i] == b'$' && i + 1 < len {
            match bytes[i + 1] {
                b'$' => {
                    result.push('$');
                    i += 2;
                }
                b'&' => {
                    result.push_str(matched);
                    i += 2;
                }
                b'`' => {
                    result.push_str(&string[..match_pos]);
                    i += 2;
                }
                b'\'' => {
                    let after = match_pos + matched.len();
                    if after < string.len() {
                        result.push_str(&string[after..]);
                    }
                    i += 2;
                }
                b'0'..=b'9' => {
                    // $n or $nn — for string replace (no captures), leave as-is
                    result.push('$');
                    i += 1;
                }
                _ => {
                    result.push('$');
                    i += 1;
                }
            }
        } else {
            // Safe: we handle byte-by-byte but push chars properly
            let ch = replacement[i..].chars().next().unwrap();
            result.push(ch);
            i += ch.len_utf8();
        }
    }
    result
}

// ============================================================================
// String Iterator
// ============================================================================

/// Helper to check if a UTF-16 code unit is a high surrogate
pub fn is_high_surrogate(unit: u16) -> bool {
    unit >= 0xD800 && unit <= 0xDBFF
}

/// Helper to check if a UTF-16 code unit is a low surrogate
pub fn is_low_surrogate(unit: u16) -> bool {
    unit >= 0xDC00 && unit <= 0xDFFF
}

/// Create a String iterator object that handles UTF-16 surrogate pairs correctly.
fn make_string_iterator(
    this_val: &Value,
    mm: Arc<MemoryManager>,
    _fn_proto: GcRef<JsObject>,
    string_iter_proto: GcRef<JsObject>,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    // Per §22.1.5.1: 1. RequireObjectCoercible(this) 2. ToString(this)
    let string = require_object_coercible_to_string(this_val, ncx)?;

    // Create iterator object with %StringIteratorPrototype% as prototype
    let iter = GcRef::new(JsObject::new(Value::object(string_iter_proto), mm));

    // Store the string reference and current index
    let _ = iter.set(PropertyKey::string("__string_ref__"), Value::string(string));
    let _ = iter.set(PropertyKey::string("__string_index__"), Value::number(0.0));

    Ok(Value::object(iter))
}

/// Helper to define a builtin method with correct name and length on a prototype
fn define_method<F>(
    proto: GcRef<JsObject>,
    name: &str,
    length: u32,
    f: F,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
) where
    F: Fn(&Value, &[Value], &mut NativeContext<'_>) -> Result<Value, VmError>
        + Send
        + Sync
        + 'static,
{
    proto.define_property(
        PropertyKey::string(name),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto_named(
            f,
            mm.clone(),
            fn_proto,
            name,
            length,
        )),
    );
}

/// Wire all String.prototype methods to the prototype object
pub fn init_string_prototype(
    string_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    string_iterator_proto: GcRef<JsObject>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
) {
    string_proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                // String primitive
                if let Some(s) = this_val.as_string() {
                    return Ok(Value::string(s));
                }
                // String wrapper object
                if let Some(obj) = this_val.as_object() {
                    if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
                        if let Some(s) = prim.as_string() {
                            return Ok(Value::string(s));
                        }
                    }
                }
                Err(VmError::type_error(
                    "String.prototype.toString requires that 'this' be a String",
                ))
            },
            mm.clone(),
            fn_proto,
        )),
    );
    string_proto.define_property(
        PropertyKey::string("valueOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                // String primitive
                if let Some(s) = this_val.as_string() {
                    return Ok(Value::string(s));
                }
                // String wrapper object
                if let Some(obj) = this_val.as_object() {
                    if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
                        if let Some(s) = prim.as_string() {
                            return Ok(Value::string(s));
                        }
                    }
                }
                Err(VmError::type_error(
                    "String.prototype.valueOf requires that 'this' be a String",
                ))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.length (getter) - uses UTF-16 code unit length
    string_proto.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::getter(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                if let Some(s) = this_val.as_string() {
                    Ok(Value::number(s.as_str().encode_utf16().count() as f64))
                } else {
                    Ok(Value::number(0.0))
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.charAt (ES2023 §22.1.3.1, length=1)
    define_method(string_proto, "charAt", 1, |this_val, args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        let pos_val = args.first().cloned().unwrap_or(Value::undefined());
        let pos = to_integer_or_infinity(&pos_val, ncx)?;
        let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
        if pos < 0.0 || pos >= utf16.len() as f64 || pos.is_infinite() {
            return Ok(Value::string(JsString::intern("")));
        }
        let idx = pos as usize;
        let ch = char::decode_utf16(std::iter::once(utf16[idx]))
            .next()
            .and_then(|r| r.ok())
            .map(|c| c.to_string())
            .unwrap_or_else(|| String::from_utf16_lossy(&[utf16[idx]]));
        Ok(Value::string(JsString::intern(&ch)))
    }, &mm, fn_proto);

    // String.prototype.charCodeAt (ES2023 §22.1.3.2, length=1)
    define_method(string_proto, "charCodeAt", 1, |this_val, args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        let pos_val = args.first().cloned().unwrap_or(Value::undefined());
        let pos = to_integer_or_infinity(&pos_val, ncx)?;
        let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
        if pos < 0.0 || pos >= utf16.len() as f64 || pos.is_infinite() {
            return Ok(Value::number(f64::NAN));
        }
        Ok(Value::number(utf16[pos as usize] as f64))
    }, &mm, fn_proto);

    // String.prototype.slice (ES2023 §22.1.3.22, length=2)
    define_method(string_proto, "slice", 2, |this_val, args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
        let len = utf16.len() as f64;
        let int_start = to_integer_or_infinity(&args.first().cloned().unwrap_or(Value::undefined()), ncx)?;
        let int_end = if args.get(1).map_or(true, |v| v.is_undefined()) {
            len
        } else {
            to_integer_or_infinity(&args[1], ncx)?
        };
        let from = if int_start < 0.0 {
            (len + int_start).max(0.0) as usize
        } else {
            int_start.min(len) as usize
        };
        let to = if int_end < 0.0 {
            (len + int_end).max(0.0) as usize
        } else {
            int_end.min(len) as usize
        };
        if to > from {
            let result = String::from_utf16_lossy(&utf16[from..to]);
            Ok(Value::string(JsString::intern(&result)))
        } else {
            Ok(Value::string(JsString::intern("")))
        }
    }, &mm, fn_proto);

    // String.prototype.substring (ES2023 §22.1.3.25, length=2)
    define_method(string_proto, "substring", 2, |this_val, args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
        let len = utf16.len() as f64;
        let int_start = to_integer_or_infinity(&args.first().cloned().unwrap_or(Value::undefined()), ncx)?;
        let int_end = if args.get(1).map_or(true, |v| v.is_undefined()) {
            len
        } else {
            to_integer_or_infinity(&args[1], ncx)?
        };
        let final_start = int_start.clamp(0.0, len) as usize;
        let final_end = int_end.clamp(0.0, len) as usize;
        let from = final_start.min(final_end);
        let to = final_start.max(final_end);
        let result = String::from_utf16_lossy(&utf16[from..to]);
        Ok(Value::string(JsString::intern(&result)))
    }, &mm, fn_proto);

    // String.prototype.toLowerCase (length=0)
    define_method(string_proto, "toLowerCase", 0, |this_val, _args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        Ok(Value::string(JsString::intern(&s.as_str().to_lowercase())))
    }, &mm, fn_proto);

    // String.prototype.toUpperCase (length=0)
    define_method(string_proto, "toUpperCase", 0, |this_val, _args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        Ok(Value::string(JsString::intern(&s.as_str().to_uppercase())))
    }, &mm, fn_proto);

    // String.prototype.trim (length=0)
    define_method(string_proto, "trim", 0, |this_val, _args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        Ok(Value::string(JsString::intern(es_trim(s.as_str()))))
    }, &mm, fn_proto);

    // String.prototype.trimStart (ES2019) + trimLeft (AnnexB alias, same function object)
    let trim_start_fn = Value::native_function_with_proto(
        |this_val, _args, ncx| {
            let s = require_object_coercible_to_string(this_val, ncx)?;
            Ok(Value::string(JsString::intern(es_trim_start(s.as_str()))))
        },
        mm.clone(),
        fn_proto,
    );
    // Set name and length on the shared function object (trimLeft.name === "trimStart" per spec)
    if let Some(obj) = trim_start_fn.as_object() {
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("trimStart"))),
        );
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(0.0)),
        );
    }
    string_proto.define_property(
        PropertyKey::string("trimStart"),
        PropertyDescriptor::builtin_method(trim_start_fn.clone()),
    );
    // AnnexB §B.2.3.16: trimLeft must be the same function object as trimStart
    string_proto.define_property(
        PropertyKey::string("trimLeft"),
        PropertyDescriptor::builtin_method(trim_start_fn),
    );

    // String.prototype.trimEnd (ES2019) + trimRight (AnnexB alias, same function object)
    let trim_end_fn = Value::native_function_with_proto(
        |this_val, _args, ncx| {
            let s = require_object_coercible_to_string(this_val, ncx)?;
            Ok(Value::string(JsString::intern(es_trim_end(s.as_str()))))
        },
        mm.clone(),
        fn_proto,
    );
    // Set name and length on the shared function object (trimRight.name === "trimEnd" per spec)
    if let Some(obj) = trim_end_fn.as_object() {
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("trimEnd"))),
        );
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::number(0.0)),
        );
    }
    string_proto.define_property(
        PropertyKey::string("trimEnd"),
        PropertyDescriptor::builtin_method(trim_end_fn.clone()),
    );
    // AnnexB §B.2.3.17: trimRight must be the same function object as trimEnd
    string_proto.define_property(
        PropertyKey::string("trimRight"),
        PropertyDescriptor::builtin_method(trim_end_fn),
    );

    // String.prototype.startsWith (ES2015)
    string_proto.define_property(
        PropertyKey::string("startsWith"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                // Per spec IsRegExp: ALWAYS call Get(@@match) first, even for RegExp
                if let Some(sv) = args.first() {
                    if !sv.is_null() && !sv.is_undefined() {
                        let is_regexp = is_regexp_check(sv, ncx)?;
                        if is_regexp {
                            return Err(VmError::type_error(
                                "First argument to String.prototype.startsWith must not be a regular expression",
                            ));
                        }
                    }
                }
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let search = arg_to_string(&search_val, ncx)?;
                let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
                let len = utf16.len() as f64;
                let pos = if args.get(1).map_or(true, |v| v.is_undefined()) {
                    0.0
                } else {
                    to_integer_or_infinity(&args[1], ncx)?.clamp(0.0, len)
                } as usize;
                let search_utf16: Vec<u16> = search.encode_utf16().collect();
                let search_len = search_utf16.len();
                if pos + search_len > utf16.len() {
                    return Ok(Value::boolean(false));
                }
                Ok(Value::boolean(utf16[pos..pos + search_len] == search_utf16[..]))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.endsWith (ES2015)
    string_proto.define_property(
        PropertyKey::string("endsWith"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                // Per spec IsRegExp: ALWAYS call Get(@@match) first, even for RegExp
                if let Some(sv) = args.first() {
                    if is_regexp_check(sv, ncx)? {
                        return Err(VmError::type_error(
                            "First argument to String.prototype.endsWith must not be a regular expression",
                        ));
                    }
                }
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let search = arg_to_string(&search_val, ncx)?;
                let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
                let len = utf16.len() as f64;
                let end_pos = if args.get(1).map_or(true, |v| v.is_undefined()) {
                    len
                } else {
                    to_integer_or_infinity(&args[1], ncx)?.clamp(0.0, len)
                } as usize;
                let search_utf16: Vec<u16> = search.encode_utf16().collect();
                let search_len = search_utf16.len();
                if search_len > end_pos {
                    return Ok(Value::boolean(false));
                }
                let start = end_pos - search_len;
                Ok(Value::boolean(utf16[start..end_pos] == search_utf16[..]))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.includes (ES2015)
    string_proto.define_property(
        PropertyKey::string("includes"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                // Per spec IsRegExp: ALWAYS call Get(@@match) first, even for RegExp
                if let Some(sv) = args.first() {
                    if is_regexp_check(sv, ncx)? {
                        return Err(VmError::type_error(
                            "First argument to String.prototype.includes must not be a regular expression",
                        ));
                    }
                }
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let search = arg_to_string(&search_val, ncx)?;
                let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
                let len = utf16.len() as f64;
                let pos = if args.get(1).map_or(true, |v| v.is_undefined()) {
                    0.0
                } else {
                    to_integer_or_infinity(&args[1], ncx)?.clamp(0.0, len)
                } as usize;
                let search_utf16: Vec<u16> = search.encode_utf16().collect();
                // Search for search_utf16 within utf16[pos..]
                let search_len = search_utf16.len();
                if search_len == 0 {
                    return Ok(Value::boolean(true));
                }
                if pos + search_len > utf16.len() {
                    return Ok(Value::boolean(false));
                }
                let found = (pos..=utf16.len() - search_len)
                    .any(|i| utf16[i..i + search_len] == search_utf16[..]);
                Ok(Value::boolean(found))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.repeat (ES2015, length=1)
    define_method(string_proto, "repeat", 1, |this_val, args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        let count_val = args.first().cloned().unwrap_or(Value::undefined());
        let count = to_integer_or_infinity(&count_val, ncx)?;
        if count < 0.0 || count == f64::INFINITY {
            return Err(VmError::range_error("Invalid count value"));
        }
        let n = count as usize;
        Ok(Value::string(JsString::intern(&s.as_str().repeat(n))))
    }, &mm, fn_proto);

    // String.prototype.padStart (ES2017)
    string_proto.define_property(
        PropertyKey::string("padStart"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let max_length = to_integer_or_infinity(&args.first().cloned().unwrap_or(Value::undefined()), ncx)?;
                let str_val = s.as_str();
                let str_utf16_len = str_val.encode_utf16().count();
                let target_len = max_length as usize;
                if max_length <= str_utf16_len as f64 {
                    return Ok(Value::string(s));
                }
                let fill_str = if args.get(1).map_or(true, |v| v.is_undefined()) {
                    " ".to_string()
                } else {
                    arg_to_string(&args[1], ncx)?
                };
                if fill_str.is_empty() {
                    return Ok(Value::string(s));
                }
                let pad_units_needed = target_len - str_utf16_len;
                let fill_utf16: Vec<u16> = fill_str.encode_utf16().collect();
                // Build pad by cycling fill code units, truncate to exact count
                let pad_utf16: Vec<u16> = fill_utf16.iter().copied().cycle().take(pad_units_needed).collect();
                let str_utf16: Vec<u16> = str_val.encode_utf16().collect();
                let mut result_utf16 = Vec::with_capacity(str_utf16.len() + pad_utf16.len());
                result_utf16.extend_from_slice(&pad_utf16);
                result_utf16.extend_from_slice(&str_utf16);
                let result = String::from_utf16_lossy(&result_utf16);
                Ok(Value::string(JsString::intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.padEnd (ES2017)
    string_proto.define_property(
        PropertyKey::string("padEnd"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let max_length = to_integer_or_infinity(&args.first().cloned().unwrap_or(Value::undefined()), ncx)?;
                let str_val = s.as_str();
                let str_utf16_len = str_val.encode_utf16().count();
                let target_len = max_length as usize;
                if max_length <= str_utf16_len as f64 {
                    return Ok(Value::string(s));
                }
                let fill_str = if args.get(1).map_or(true, |v| v.is_undefined()) {
                    " ".to_string()
                } else {
                    arg_to_string(&args[1], ncx)?
                };
                if fill_str.is_empty() {
                    return Ok(Value::string(s));
                }
                let pad_units_needed = target_len - str_utf16_len;
                let fill_utf16: Vec<u16> = fill_str.encode_utf16().collect();
                let pad_utf16: Vec<u16> = fill_utf16.iter().copied().cycle().take(pad_units_needed).collect();
                let str_utf16: Vec<u16> = str_val.encode_utf16().collect();
                let mut result_utf16 = Vec::with_capacity(str_utf16.len() + pad_utf16.len());
                result_utf16.extend_from_slice(&str_utf16);
                result_utf16.extend_from_slice(&pad_utf16);
                let result = String::from_utf16_lossy(&result_utf16);
                Ok(Value::string(JsString::intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.at (ES2022)
    string_proto.define_property(
        PropertyKey::string("at"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
                let len = utf16.len() as f64;
                let rel_idx = to_integer_or_infinity(&args.first().cloned().unwrap_or(Value::undefined()), ncx)?;
                let k = if rel_idx < 0.0 { len + rel_idx } else { rel_idx };
                if k < 0.0 || k >= len {
                    return Ok(Value::undefined());
                }
                let idx = k as usize;
                let result = String::from_utf16_lossy(&utf16[idx..idx + 1]);
                Ok(Value::string(JsString::intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.indexOf
    string_proto.define_property(
        PropertyKey::string("indexOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let search = arg_to_string(&search_val, ncx)?;
                let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
                let len = utf16.len() as f64;
                let pos = if args.get(1).map_or(true, |v| v.is_undefined()) {
                    0.0
                } else {
                    to_integer_or_infinity(&args[1], ncx)?.clamp(0.0, len)
                } as usize;
                let search_utf16: Vec<u16> = search.encode_utf16().collect();
                let search_len = search_utf16.len();
                if search_len == 0 {
                    return Ok(Value::number(pos.min(utf16.len()) as f64));
                }
                if pos + search_len > utf16.len() {
                    return Ok(Value::number(-1.0));
                }
                for i in pos..=utf16.len() - search_len {
                    if utf16[i..i + search_len] == search_utf16[..] {
                        return Ok(Value::number(i as f64));
                    }
                }
                Ok(Value::number(-1.0))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.lastIndexOf
    string_proto.define_property(
        PropertyKey::string("lastIndexOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let search = arg_to_string(&search_val, ncx)?;
                let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
                let len = utf16.len();
                let num_pos = if args.get(1).map_or(true, |v| v.is_undefined()) {
                    f64::NAN
                } else {
                    ncx.to_number_value(&args[1])?
                };
                let pos = if num_pos.is_nan() {
                    len
                } else {
                    num_pos.clamp(0.0, len as f64) as usize
                };
                let search_utf16: Vec<u16> = search.encode_utf16().collect();
                let search_len = search_utf16.len();
                if search_len == 0 {
                    return Ok(Value::number(pos.min(len) as f64));
                }
                if search_len > len {
                    return Ok(Value::number(-1.0));
                }
                let max_start = pos.min(len - search_len);
                for i in (0..=max_start).rev() {
                    if utf16[i..i + search_len] == search_utf16[..] {
                        return Ok(Value::number(i as f64));
                    }
                }
                Ok(Value::number(-1.0))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.concat
    string_proto.define_property(
        PropertyKey::string("concat"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let mut result = s.as_str().to_string();
                for arg in args {
                    let arg_str = ncx.to_string_value(arg)?;
                    result.push_str(&arg_str);
                }
                Ok(Value::string(JsString::intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.split (ES2026 §22.1.3.22)
    string_proto.define_property(
        PropertyKey::string("split"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                // 1. RequireObjectCoercible(this)
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error(
                        "String.prototype.split called on null or undefined",
                    ));
                }

                // 2. Check for @@split on separator (any object, not just RegExp)
                if let Some(sep) = args.first() {
                    if !sep.is_null() && !sep.is_undefined() {
                        if sep.as_regex().is_some() || sep.as_object().is_some() {
                            let method = get_property_of(
                                sep,
                                &PropertyKey::Symbol(crate::intrinsics::well_known::split_symbol()),
                                ncx,
                            )?;
                            if method.is_callable() {
                                // Per spec: pass raw O (RequireObjectCoercible), NOT ToString(O)
                                let mut sym_args = vec![this_val.clone()];
                                if let Some(limit) = args.get(1) {
                                    sym_args.push(limit.clone());
                                }
                                return ncx.call_function(&method, sep.clone(), &sym_args);
                            }
                        }
                    }
                }

                // 3. ToString(this)
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let str_val = s.as_str();
                let separator = args.first();

                // Per spec: ToUint32(limit) BEFORE ToString(separator)
                // 4. Let lim = limit is undefined ? 2^32-1 : ToUint32(limit)
                let limit_val = args.get(1).cloned().unwrap_or(Value::undefined());
                let limit: Option<u32> = if limit_val.is_undefined() {
                    None
                } else {
                    let n = ncx.to_number_value(&limit_val)?;
                    // Proper ToUint32: modulo 2^32
                    let uint32 = if n.is_nan() || n.is_infinite() || n == 0.0 {
                        0u32
                    } else {
                        (n.trunc().rem_euclid(4294967296.0)) as u32
                    };
                    Some(uint32)
                };

                // 5. Let R = ToString(separator)
                let sep_string: Option<String> = if let Some(sep) = separator {
                    if sep.is_undefined() {
                        None
                    } else {
                        Some(arg_to_string(sep, ncx)?)
                    }
                } else {
                    None
                };

                // If limit is 0, return empty array
                if limit == Some(0) {
                    let result = create_array(ncx, 0);
                    return Ok(Value::array(result));
                }

                let parts: Vec<String> = if let Some(ref sep_str) = sep_string {
                    if sep_str.is_empty() {
                        // Split into individual UTF-16 code units
                        let utf16: Vec<u16> = str_val.encode_utf16().collect();
                        utf16.iter().map(|&u| {
                            String::from(char::from_u32(u as u32).unwrap_or('\u{FFFD}'))
                        }).collect()
                    } else {
                        str_val.split(sep_str.as_str()).map(|s| s.to_string()).collect()
                    }
                } else {
                    vec![str_val.to_string()]
                };

                let max = limit.map(|l| l as usize).unwrap_or(parts.len());
                let result_len = max.min(parts.len());
                let result = create_array(ncx, result_len);
                for (i, part) in parts.iter().take(result_len).enumerate() {
                    let _ = result.set(
                        PropertyKey::Index(i as u32),
                        Value::string(JsString::intern(part)),
                    );
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.replace (ES2026 §22.1.3.19)
    string_proto.define_property(
        PropertyKey::string("replace"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                // 1. RequireObjectCoercible(this)
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error(
                        "String.prototype.replace called on null or undefined",
                    ));
                }

                // 2. Check for @@replace on any object (RegExp or custom)
                if let Some(search_val) = args.first() {
                    if !search_val.is_null() && !search_val.is_undefined() {
                        if search_val.as_regex().is_some() || search_val.as_object().is_some() {
                            let method = get_property_of(
                                search_val,
                                &PropertyKey::Symbol(crate::intrinsics::well_known::replace_symbol()),
                                ncx,
                            )?;
                            if method.is_callable() {
                                // Per spec: Call(replacer, searchValue, « O, replaceValue »)
                                // O is the raw this value, not ToString'd
                                let mut sym_args = vec![this_val.clone()];
                                if let Some(replacement) = args.get(1) {
                                    sym_args.push(replacement.clone());
                                }
                                return ncx.call_function(&method, search_val.clone(), &sym_args);
                            }
                        }
                    }
                }

                // 3. String-based replace (first occurrence only)
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let str_val = s.as_str();
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let search = arg_to_string(&search_val, ncx)?;
                // Check if replacement is a function
                let replace_val = args.get(1).cloned().unwrap_or(Value::undefined());
                let replacement = if replace_val.is_callable() {
                    // Call replacer function with (match, offset, string)
                    if let Some(pos) = str_val.find(&*search) {
                        let call_args = [
                            Value::string(JsString::intern(&search)),
                            Value::number(pos as f64),
                            Value::string(s.clone()),
                        ];
                        let result = ncx.call_function(&replace_val, Value::undefined(), &call_args)?;
                        let result_str = ncx.to_string_value(&result)?;
                        let replaced = format!(
                            "{}{}{}",
                            &str_val[..pos],
                            result_str,
                            &str_val[pos + search.len()..]
                        );
                        return Ok(Value::string(JsString::intern(&replaced)));
                    } else {
                        return Ok(Value::string(s));
                    }
                } else {
                    arg_to_string(&replace_val, ncx)?
                };

                if let Some(pos) = str_val.find(&*search) {
                    let substituted = apply_replacement_pattern(&replacement, &search, str_val, pos);
                    let result = format!(
                        "{}{}{}",
                        &str_val[..pos],
                        substituted,
                        &str_val[pos + search.len()..]
                    );
                    Ok(Value::string(JsString::intern(&result)))
                } else {
                    Ok(Value::string(s))
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.replaceAll (ES2021 §22.1.3.20)
    string_proto.define_property(
            PropertyKey::string("replaceAll"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    // 1. RequireObjectCoercible(this)
                    if this_val.is_null() || this_val.is_undefined() {
                        return Err(VmError::type_error(
                            "String.prototype.replaceAll called on null or undefined",
                        ));
                    }

                    // 2. If searchValue is not null/undefined, check for IsRegExp and @@replace
                    if let Some(search_val) = args.first() {
                        if !search_val.is_null() && !search_val.is_undefined() {
                            // Per spec IsRegExp: ALWAYS call Get(@@match) first
                            let is_regexp = is_regexp_check(search_val, ncx)?;
                            if is_regexp {
                                // Per spec: ALWAYS use Get(searchValue, "flags")
                                let flags_val = get_property_of(
                                    search_val,
                                    &PropertyKey::string("flags"),
                                    ncx,
                                )?;
                                // RequireObjectCoercible(flags)
                                if flags_val.is_null() || flags_val.is_undefined() {
                                    return Err(VmError::type_error(
                                        "Cannot convert undefined or null to object",
                                    ));
                                }
                                let flags_str = arg_to_string(&flags_val, ncx)?;
                                if !flags_str.contains('g') {
                                    return Err(VmError::type_error(
                                        "String.prototype.replaceAll called with a non-global RegExp argument",
                                    ));
                                }
                            }
                            // Check for @@replace on any object (via getter)
                            let method = get_property_of(
                                search_val,
                                &PropertyKey::Symbol(crate::intrinsics::well_known::replace_symbol()),
                                ncx,
                            )?;
                            if !method.is_undefined() && !method.is_null() {
                                if !method.is_callable() {
                                    return Err(VmError::type_error(
                                        "Symbol.replace is not a function",
                                    ));
                                }
                                // Per spec: Call(replacer, searchValue, « O, replaceValue »)
                                let mut sym_args = vec![this_val.clone()];
                                if let Some(replacement) = args.get(1) {
                                    sym_args.push(replacement.clone());
                                }
                                return ncx.call_function(&method, search_val.clone(), &sym_args);
                            }
                        }
                    }

                    // 3. String-based replaceAll
                    let s = require_object_coercible_to_string(this_val, ncx)?;
                    let str_val = s.as_str();
                    let search_val = args.first().cloned().unwrap_or(Value::undefined());
                    let search = arg_to_string(&search_val, ncx)?;
                    let replace_val = args.get(1).cloned().unwrap_or(Value::undefined());

                    if replace_val.is_callable() {
                        // Function replacer for replaceAll
                        let mut result = String::new();
                        let mut last_end = 0;
                        while let Some(pos) = str_val[last_end..].find(&*search) {
                            let abs_pos = last_end + pos;
                            result.push_str(&str_val[last_end..abs_pos]);
                            let call_args = [
                                Value::string(JsString::intern(&search)),
                                Value::number(abs_pos as f64),
                                Value::string(s.clone()),
                            ];
                            let rep = ncx.call_function(&replace_val, Value::undefined(), &call_args)?;
                            let rep_str = ncx.to_string_value(&rep)?;
                            result.push_str(&rep_str);
                            last_end = abs_pos + search.len();
                            if search.is_empty() {
                                if last_end < str_val.len() {
                                    // Advance one char for empty search
                                    let ch = str_val[last_end..].chars().next().unwrap();
                                    result.push(ch);
                                    last_end += ch.len_utf8();
                                } else {
                                    break;
                                }
                            }
                        }
                        result.push_str(&str_val[last_end..]);
                        Ok(Value::string(JsString::intern(&result)))
                    } else {
                        let replacement = arg_to_string(&replace_val, ncx)?;
                        // Apply replacement patterns for each match
                        let mut result = String::new();
                        let mut last_end = 0;
                        while let Some(pos) = str_val[last_end..].find(&*search) {
                            let abs_pos = last_end + pos;
                            result.push_str(&str_val[last_end..abs_pos]);
                            let substituted = apply_replacement_pattern(&replacement, &search, str_val, abs_pos);
                            result.push_str(&substituted);
                            last_end = abs_pos + search.len();
                            if search.is_empty() {
                                if last_end < str_val.len() {
                                    let ch = str_val[last_end..].chars().next().unwrap();
                                    result.push(ch);
                                    last_end += ch.len_utf8();
                                } else {
                                    break;
                                }
                            }
                        }
                        result.push_str(&str_val[last_end..]);
                        Ok(Value::string(JsString::intern(&result)))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
        );

    // String.prototype.search (ES2026 §22.1.3.21)
    string_proto.define_property(
        PropertyKey::string("search"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                // 1. RequireObjectCoercible(this)
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error(
                        "String.prototype.search called on null or undefined",
                    ));
                }

                // 2. If regexp is not null/undefined, check for @@search on any object
                if let Some(search_val) = args.first() {
                    if !search_val.is_null() && !search_val.is_undefined() {
                        if search_val.as_regex().is_some() || search_val.as_object().is_some() {
                            let method = get_property_of(
                                search_val,
                                &PropertyKey::Symbol(crate::intrinsics::well_known::search_symbol()),
                                ncx,
                            )?;
                            if method.is_callable() {
                                let s = require_object_coercible_to_string(this_val, ncx)?;
                                return ncx.call_function(&method, search_val.clone(), &[Value::string(s)]);
                            }
                        }
                    }
                }

                // 3. ToString(this)
                let s = require_object_coercible_to_string(this_val, ncx)?;

                // 4. Let regexp be ? RegExpCreate(regexp, undefined)
                // Per spec: pass the raw value to RegExp constructor (not ToString'd)
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let regexp_ctor = ncx.ctx.get_global("RegExp");
                if let Some(ctor) = regexp_ctor {
                    let ctor_args = [search_val.clone()];
                    let regex_val = ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                    let rx_obj = regex_val.as_regex().map(|r| r.object.clone())
                        .or_else(|| regex_val.as_object());
                    if let Some(obj) = rx_obj {
                        let method = obj
                            .get(&PropertyKey::Symbol(crate::intrinsics::well_known::search_symbol()))
                            .unwrap_or_else(Value::undefined);
                        if method.is_callable() {
                            return ncx.call_function(&method, regex_val, &[Value::string(s.clone())]);
                        }
                    }
                }
                // Fallback: simple indexOf
                let search_str = arg_to_string(&search_val, ncx)?;
                let str_val = s.as_str();
                match str_val.find(&search_str) {
                    Some(pos) => Ok(Value::int32(pos as i32)),
                    None => Ok(Value::int32(-1)),
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.match (ES2026 §22.1.3.12)
    string_proto.define_property(
        PropertyKey::string("match"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                // 1. RequireObjectCoercible(this)
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error(
                        "String.prototype.match called on null or undefined",
                    ));
                }

                // 2. If regexp is not null/undefined, check for @@match on any object
                if let Some(search_val) = args.first() {
                    if !search_val.is_null() && !search_val.is_undefined() {
                        if search_val.as_regex().is_some() || search_val.as_object().is_some() {
                            let method = get_property_of(
                                search_val,
                                &PropertyKey::Symbol(crate::intrinsics::well_known::match_symbol()),
                                ncx,
                            )?;
                            if method.is_callable() {
                                let s = require_object_coercible_to_string(this_val, ncx)?;
                                return ncx.call_function(&method, search_val.clone(), &[Value::string(s)]);
                            }
                        }
                    }
                }

                // 3. ToString(this)
                let s = require_object_coercible_to_string(this_val, ncx)?;

                // 4. Let rx be ? RegExpCreate(regexp, undefined)
                let search_val = args.first().cloned().unwrap_or(Value::undefined());
                let regexp_ctor = ncx.ctx.get_global("RegExp");
                if let Some(ctor) = regexp_ctor {
                    let ctor_args = [search_val.clone()];
                    let regex_val = ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                    let rx_obj = regex_val.as_regex().map(|r| r.object.clone())
                        .or_else(|| regex_val.as_object());
                    if let Some(obj) = rx_obj {
                        let method = obj
                            .get(&PropertyKey::Symbol(crate::intrinsics::well_known::match_symbol()))
                            .unwrap_or_else(Value::undefined);
                        if method.is_callable() {
                            return ncx.call_function(&method, regex_val, &[Value::string(s.clone())]);
                        }
                    }
                }
                // Fallback: simple indexOf
                let fallback_str = arg_to_string(&search_val, ncx)?;
                let str_val = s.as_str();
                match str_val.find(&fallback_str) {
                    Some(pos) => {
                        let arr = create_array(ncx, 1);
                        let _ = arr.set(
                            PropertyKey::Index(0),
                            Value::string(JsString::intern(&fallback_str)),
                        );
                        let _ = arr.set(PropertyKey::string("index"), Value::number(pos as f64));
                        let _ = arr.set(PropertyKey::string("input"), Value::string(s.clone()));
                        let _ = arr.set(PropertyKey::string("groups"), Value::undefined());
                        Ok(Value::array(arr))
                    }
                    None => Ok(Value::null()),
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.matchAll (ES2020 §22.1.3.13)
    string_proto.define_property(
            PropertyKey::string("matchAll"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    // 1. RequireObjectCoercible(this)
                    if this_val.is_null() || this_val.is_undefined() {
                        return Err(VmError::type_error(
                            "String.prototype.matchAll called on null or undefined",
                        ));
                    }

                    // 2. If regexp is not null/undefined, check IsRegExp and @@matchAll
                    if let Some(search_val) = args.first() {
                        if !search_val.is_null() && !search_val.is_undefined() {
                            if search_val.as_regex().is_some() || search_val.as_object().is_some() {
                                // Check IsRegExp via Symbol.match (using getter-aware access)
                                let is_regexp = search_val.as_regex().is_some() || {
                                    let match_val = get_property_of(
                                        search_val,
                                        &PropertyKey::Symbol(crate::intrinsics::well_known::match_symbol()),
                                        ncx,
                                    )?;
                                    !match_val.is_undefined() && !match_val.is_null()
                                };
                                if is_regexp {
                                    // Per spec: ALWAYS use Get(regexp, "flags")
                                    let flags_val = get_property_of(
                                        search_val,
                                        &PropertyKey::string("flags"),
                                        ncx,
                                    )?;
                                    // RequireObjectCoercible(flags)
                                    if flags_val.is_null() || flags_val.is_undefined() {
                                        return Err(VmError::type_error(
                                            "Cannot convert undefined or null to object",
                                        ));
                                    }
                                    let flags_str = arg_to_string(&flags_val, ncx)?;
                                    if !flags_str.contains('g') {
                                        return Err(VmError::type_error(
                                            "String.prototype.matchAll called with a non-global RegExp argument",
                                        ));
                                    }
                                }
                                // Check for @@matchAll on any object (getter-aware)
                                let method = get_property_of(
                                    search_val,
                                    &PropertyKey::Symbol(crate::intrinsics::well_known::match_all_symbol()),
                                    ncx,
                                )?;
                                if method.is_callable() {
                                    let s = require_object_coercible_to_string(this_val, ncx)?;
                                    return ncx.call_function(&method, search_val.clone(), &[Value::string(s)]);
                                }
                                // Per spec: if @@matchAll is not callable and not undefined, throw TypeError
                                if !method.is_undefined() && !method.is_null() {
                                    return Err(VmError::type_error(
                                        "Symbol.matchAll is not a function",
                                    ));
                                }
                            }
                        }
                    }

                    // 3. ToString(this), then create RegExp and delegate
                    let s = require_object_coercible_to_string(this_val, ncx)?;
                    let search_val = args.first().cloned().unwrap_or(Value::undefined());
                    // Per spec: pass raw value to RegExp, not ToString'd
                    let regexp_ctor = ncx.ctx.get_global("RegExp");
                    if let Some(ctor) = regexp_ctor {
                        let ctor_args = [
                            search_val,
                            Value::string(JsString::intern("g")),
                        ];
                        let regex_val = ncx.call_function_construct(&ctor, Value::undefined(), &ctor_args)?;
                        // Invoke(rx, @@matchAll, « S ») - throws if method not found
                        let method = get_property_of(
                            &regex_val,
                            &PropertyKey::Symbol(crate::intrinsics::well_known::match_all_symbol()),
                            ncx,
                        )?;
                        if method.is_callable() {
                            return ncx.call_function(&method, regex_val, &[Value::string(s.clone())]);
                        }
                        // Per spec Invoke: if method is not callable, throw TypeError
                        return Err(VmError::type_error("Symbol.matchAll is not a function"));
                    }
                    Err(VmError::type_error("RegExp constructor not found"))
                },
                mm.clone(),
                fn_proto,
            )),
        );

    // String.prototype.codePointAt(pos) — §22.1.3.3
    string_proto.define_property(
        PropertyKey::string("codePointAt"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let str_val = s.as_str();
                let pos_val = args.first().cloned().unwrap_or(Value::undefined());
                let pos = to_integer_or_infinity(&pos_val, ncx)?;

                let utf16: Vec<u16> = str_val.encode_utf16().collect();
                if pos < 0.0 || pos >= utf16.len() as f64 {
                    return Ok(Value::undefined());
                }
                let pos = pos as usize;

                let first = utf16[pos];
                if is_high_surrogate(first) && pos + 1 < utf16.len() {
                    let second = utf16[pos + 1];
                    if is_low_surrogate(second) {
                        // Combine surrogate pair into code point
                        let cp = ((first as u32 - 0xD800) * 0x400
                            + (second as u32 - 0xDC00)
                            + 0x10000) as f64;
                        return Ok(Value::number(cp));
                    }
                }
                Ok(Value::number(first as f64))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.normalize([form]) — §22.1.3.13
    string_proto.define_property(
        PropertyKey::string("normalize"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                use unicode_normalization::UnicodeNormalization;
                let s = require_object_coercible_to_string(this_val, ncx)?;
                let str_val = s.as_str();
                let f_str = if args.first().map_or(true, |v| v.is_undefined()) {
                    "NFC".to_string()
                } else {
                    arg_to_string(&args[0], ncx)?
                };
                match f_str.as_str() {
                    "NFC" => Ok(Value::string(JsString::intern(&str_val.nfc().collect::<String>()))),
                    "NFD" => Ok(Value::string(JsString::intern(&str_val.nfd().collect::<String>()))),
                    "NFKC" => Ok(Value::string(JsString::intern(&str_val.nfkc().collect::<String>()))),
                    "NFKD" => Ok(Value::string(JsString::intern(&str_val.nfkd().collect::<String>()))),
                    _ => Err(VmError::range_error(
                        &format!("The normalization form should be one of NFC, NFD, NFKC, NFKD. Got: {}", f_str)
                    )),
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // String.prototype.isWellFormed (ES2024)
    define_method(string_proto, "isWellFormed", 0, |this_val, _args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
        let mut i = 0;
        while i < utf16.len() {
            let code = utf16[i];
            if is_high_surrogate(code) {
                if i + 1 >= utf16.len() || !is_low_surrogate(utf16[i + 1]) {
                    return Ok(Value::boolean(false));
                }
                i += 2;
            } else if is_low_surrogate(code) {
                return Ok(Value::boolean(false));
            } else {
                i += 1;
            }
        }
        Ok(Value::boolean(true))
    }, &mm, fn_proto);

    // String.prototype.toWellFormed (ES2024)
    define_method(string_proto, "toWellFormed", 0, |this_val, _args, ncx| {
        let s = require_object_coercible_to_string(this_val, ncx)?;
        let utf16: Vec<u16> = s.as_str().encode_utf16().collect();
        let mut result: Vec<u16> = Vec::with_capacity(utf16.len());
        let mut i = 0;
        while i < utf16.len() {
            let code = utf16[i];
            if is_high_surrogate(code) {
                if i + 1 < utf16.len() && is_low_surrogate(utf16[i + 1]) {
                    result.push(code);
                    result.push(utf16[i + 1]);
                    i += 2;
                } else {
                    result.push(0xFFFD); // U+FFFD REPLACEMENT CHARACTER
                    i += 1;
                }
            } else if is_low_surrogate(code) {
                result.push(0xFFFD);
                i += 1;
            } else {
                result.push(code);
                i += 1;
            }
        }
        let s = String::from_utf16_lossy(&result);
        Ok(Value::string(JsString::intern(&s)))
    }, &mm, fn_proto);

    // ========================================================================
    // Annex B: String.prototype.substr (§B.2.3.1)
    // ========================================================================
    {
        let func = Value::native_function_with_proto(
            |this_val, args, ncx| {
                // 1. RequireObjectCoercible(this)
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error(
                        "String.prototype.substr called on null or undefined",
                    ));
                }
                // 2. Let S = ToString(this) — propagates toString() exceptions
                let s = ncx.to_string_value(this_val)?;
                // Work in UTF-16 code units for spec-correct indexing
                let utf16: Vec<u16> = s.encode_utf16().collect();
                let size = utf16.len() as i64;

                // 3. ToIntegerOrInfinity(start)
                let start_val = args.first().cloned().unwrap_or(Value::undefined());
                let start_num = ncx.to_number_value(&start_val)?;
                let int_start: i64 = if start_num.is_nan() {
                    0
                } else if start_num == f64::INFINITY {
                    size // min(+∞, size)
                } else if start_num == f64::NEG_INFINITY {
                    0 // max(size + -∞, 0) = 0
                } else {
                    start_num.trunc() as i64
                };

                // 4. ToIntegerOrInfinity(length); undefined → treat as +∞
                let length_val = args.get(1).cloned().unwrap_or(Value::undefined());
                let int_length: Option<i64> = if length_val.is_undefined() {
                    None // +∞: go to end
                } else {
                    let n = ncx.to_number_value(&length_val)?;
                    if n.is_nan() {
                        Some(0)
                    } else if n == f64::INFINITY {
                        None // +∞
                    } else if n == f64::NEG_INFINITY {
                        Some(i64::MIN) // -∞ → length 0 or negative
                    } else {
                        Some(n.trunc() as i64)
                    }
                };

                // 5. Clamp intStart
                let start = if int_start < 0 {
                    (size + int_start).max(0)
                } else {
                    int_start.min(size)
                } as usize;

                // 6. Compute end
                let end = match int_length {
                    None => size as usize,
                    Some(l) if l <= 0 => {
                        return Ok(Value::string(JsString::intern("")));
                    }
                    Some(l) => {
                        let raw = start as i64 + l;
                        if raw <= 0 {
                            return Ok(Value::string(JsString::intern("")));
                        }
                        raw.min(size) as usize
                    }
                };

                if end <= start {
                    return Ok(Value::string(JsString::intern("")));
                }

                // Use intern_utf16 to preserve lone surrogates (e.g. substr(1)
                // on '\ud834\udf06' must return the lone low surrogate '\udf06').
                // from_utf16_lossy would replace lone surrogates with U+FFFD.
                Ok(Value::string(JsString::intern_utf16(&utf16[start..end])))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern("substr"))),
            );
            obj.define_property(
                PropertyKey::string("length"),
                PropertyDescriptor::function_length(Value::number(2.0)),
            );
        }
        string_proto.define_property(
            PropertyKey::string("substr"),
            PropertyDescriptor::builtin_method(func),
        );
    }

    // ========================================================================
    // Annex B: String.prototype HTML wrapper methods (§B.2.3.2)
    // All follow the CreateHTML abstract operation:
    //   1. RequireObjectCoercible(this)  → TypeError for null/undefined
    //   2. ToString(this)               → calls toString(), propagates errors
    //   3. Return "<tag>" + S + "</tag>"
    // ========================================================================

    // No-argument methods (length = 0)
    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.big called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<big>{s}</big>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("big"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("big"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.blink called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<blink>{s}</blink>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("blink"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("blink"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.bold called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<b>{s}</b>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("bold"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("bold"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.fixed called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<tt>{s}</tt>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("fixed"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("fixed"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.italics called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<i>{s}</i>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("italics"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("italics"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.small called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<small>{s}</small>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("small"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("small"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.strike called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<strike>{s}</strike>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("strike"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("strike"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.sub called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<sub>{s}</sub>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("sub"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("sub"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, _args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.sup called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                Ok(Value::string(JsString::intern(&format!("<sup>{s}</sup>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("sup"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(0.0)));
        }
        string_proto.define_property(PropertyKey::string("sup"), PropertyDescriptor::builtin_method(func));
    }

    // Methods with one attribute argument (length = 1) — attribute value has " escaped to &quot;
    {
        let func = Value::native_function_with_proto(
            |this_val, args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.anchor called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                let name_val = args.first().cloned().unwrap_or(Value::undefined());
                let name = ncx.to_string_value(&name_val)?;
                let escaped = name.replace('"', "&quot;");
                Ok(Value::string(JsString::intern(&format!("<a name=\"{escaped}\">{s}</a>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("anchor"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(1.0)));
        }
        string_proto.define_property(PropertyKey::string("anchor"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.fontcolor called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                let color_val = args.first().cloned().unwrap_or(Value::undefined());
                let color = ncx.to_string_value(&color_val)?;
                let escaped = color.replace('"', "&quot;");
                Ok(Value::string(JsString::intern(&format!("<font color=\"{escaped}\">{s}</font>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("fontcolor"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(1.0)));
        }
        string_proto.define_property(PropertyKey::string("fontcolor"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.fontsize called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                let size_val = args.first().cloned().unwrap_or(Value::undefined());
                let size = ncx.to_string_value(&size_val)?;
                let escaped = size.replace('"', "&quot;");
                Ok(Value::string(JsString::intern(&format!("<font size=\"{escaped}\">{s}</font>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("fontsize"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(1.0)));
        }
        string_proto.define_property(PropertyKey::string("fontsize"), PropertyDescriptor::builtin_method(func));
    }

    {
        let func = Value::native_function_with_proto(
            |this_val, args, ncx| {
                if this_val.is_null() || this_val.is_undefined() {
                    return Err(VmError::type_error("String.prototype.link called on null or undefined"));
                }
                let s = ncx.to_string_value(this_val)?;
                let href_val = args.first().cloned().unwrap_or(Value::undefined());
                let href = ncx.to_string_value(&href_val)?;
                let escaped = href.replace('"', "&quot;");
                Ok(Value::string(JsString::intern(&format!("<a href=\"{escaped}\">{s}</a>"))))
            },
            mm.clone(),
            fn_proto,
        );
        if let Some(obj) = func.as_object() {
            obj.define_property(PropertyKey::string("name"), PropertyDescriptor::function_length(Value::string(JsString::intern("link"))));
            obj.define_property(PropertyKey::string("length"), PropertyDescriptor::function_length(Value::number(1.0)));
        }
        string_proto.define_property(PropertyKey::string("link"), PropertyDescriptor::builtin_method(func));
    }

    // String.prototype[Symbol.iterator]
    let iter_proto_for_symbol = string_iterator_proto;
    let mm_for_symbol = mm.clone();
    let fn_proto_for_symbol = fn_proto;
    string_proto.define_property(
        PropertyKey::Symbol(symbol_iterator),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                make_string_iterator(
                    this_val,
                    ncx.memory_manager().clone(),
                    fn_proto_for_symbol,
                    iter_proto_for_symbol,
                    ncx,
                )
            },
            mm_for_symbol,
            fn_proto,
        )),
    );

    // Bulk fixup: correct name/length properties for all String.prototype methods
    // Per ES2023 §22.1.3, each method has a specific `length` value
    let method_lengths: &[(&str, u32)] = &[
        ("toString", 0),
        ("valueOf", 0),
        ("startsWith", 1),
        ("endsWith", 1),
        ("includes", 1),
        ("padStart", 1),
        ("padEnd", 1),
        ("indexOf", 1),
        ("lastIndexOf", 1),
        ("concat", 1),
        ("split", 2),
        ("replace", 2),
        ("replaceAll", 2),
        ("search", 1),
        ("match", 1),
        ("matchAll", 1),
        ("codePointAt", 1),
        ("normalize", 0),
        ("at", 1),
        ("substr", 2),
        ("toLocaleLowerCase", 0),
        ("toLocaleUpperCase", 0),
        ("localeCompare", 1),
    ];
    for (name, length) in method_lengths {
        if let Some(func_val) = string_proto.get(&PropertyKey::string(name)) {
            if let Some(func_obj) = func_val.native_function_object() {
                func_obj.define_property(
                    PropertyKey::string("name"),
                    PropertyDescriptor::function_length(Value::string(JsString::intern(name))),
                );
                func_obj.define_property(
                    PropertyKey::string("length"),
                    PropertyDescriptor::function_length(Value::number(*length as f64)),
                );
            }
        }
    }

    // Fix name/length for Symbol.iterator method
    let iter_sym = crate::intrinsics::well_known::iterator_symbol();
    if let Some(func_val) = string_proto.get(&PropertyKey::Symbol(iter_sym)) {
        if let Some(func_obj) = func_val.native_function_object() {
            func_obj.define_property(
                PropertyKey::string("name"),
                PropertyDescriptor::function_length(Value::string(JsString::intern("[Symbol.iterator]"))),
            );
            func_obj.define_property(
                PropertyKey::string("length"),
                PropertyDescriptor::function_length(Value::number(0.0)),
            );
        }
    }
}
