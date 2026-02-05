//! JSON namespace initialization (ES2026 §25.5)
//!
//! Creates the JSON global namespace object with:
//! - JSON.parse(text, reviver?) — Parse JSON text into a JavaScript value
//! - JSON.stringify(value, replacer?, space?) — Serialize a JavaScript value to JSON
//!
//! ## ES2026 Compliance
//!
//! The JSON object has [[Prototype]] of Object.prototype and is not a constructor.
//! Both methods are non-enumerable.

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use std::collections::HashSet;
use std::sync::Arc;

/// Convert serde_json::Value to JavaScript Value
/// object_proto and array_proto are used to set proper prototypes on created objects
fn json_to_value(
    json: &serde_json::Value,
    mm: &Arc<MemoryManager>,
    object_proto: &Value,
    array_proto: &Value,
) -> Value {
    match json {
        serde_json::Value::Null => Value::null(),
        serde_json::Value::Bool(b) => Value::boolean(*b),
        serde_json::Value::Number(n) => Value::number(n.as_f64().unwrap_or(f64::NAN)),
        serde_json::Value::String(s) => Value::string(JsString::intern(s)),
        serde_json::Value::Array(items) => {
            let arr = GcRef::new(JsObject::array(items.len(), mm.clone()));
            // Set Array.prototype
            arr.set_prototype(array_proto.clone());
            for (i, item) in items.iter().enumerate() {
                arr.set(
                    PropertyKey::Index(i as u32),
                    json_to_value(item, mm, object_proto, array_proto),
                );
            }
            Value::array(arr)
        }
        serde_json::Value::Object(map) => {
            let obj = GcRef::new(JsObject::new(object_proto.clone(), mm.clone()));
            for (k, v) in map {
                obj.set(
                    PropertyKey::string(k),
                    json_to_value(v, mm, object_proto, array_proto),
                );
            }
            Value::object(obj)
        }
    }
}

/// Escape special characters in JSON strings (using UTF-8 input)
fn escape_json_string(s: &str) -> String {
    let mut result = String::with_capacity(s.len());
    for c in s.chars() {
        let code = c as u32;
        match c {
            '"' => result.push_str("\\\""),
            '\\' => result.push_str("\\\\"),
            '\n' => result.push_str("\\n"),
            '\r' => result.push_str("\\r"),
            '\t' => result.push_str("\\t"),
            '\x08' => result.push_str("\\b"),
            '\x0C' => result.push_str("\\f"),
            c if code < 0x20 => result.push_str(&format!("\\u{:04x}", code)),
            c => result.push(c),
        }
    }
    result
}

/// Escape JSON string preserving lone surrogates from UTF-16 data
fn escape_json_string_utf16(units: &[u16]) -> String {
    let mut result = String::with_capacity(units.len() * 2);
    let mut i = 0;
    while i < units.len() {
        let code = units[i];
        match code {
            0x22 => result.push_str("\\\""),       // "
            0x5C => result.push_str("\\\\"),       // \
            0x0A => result.push_str("\\n"),        // \n
            0x0D => result.push_str("\\r"),        // \r
            0x09 => result.push_str("\\t"),        // \t
            0x08 => result.push_str("\\b"),        // \b
            0x0C => result.push_str("\\f"),        // \f
            c if c < 0x20 => result.push_str(&format!("\\u{:04x}", c)),
            // High surrogate
            c if (0xD800..=0xDBFF).contains(&c) => {
                // Check for valid surrogate pair
                if i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
                    // Valid pair - decode to code point and output as UTF-8
                    let high = (c as u32 - 0xD800) << 10;
                    let low = units[i + 1] as u32 - 0xDC00;
                    let cp = 0x10000 + high + low;
                    if let Some(ch) = char::from_u32(cp) {
                        result.push(ch);
                    }
                    i += 1; // Skip the low surrogate
                } else {
                    // Lone high surrogate - escape it
                    result.push_str(&format!("\\u{:04x}", c));
                }
            }
            // Lone low surrogate - escape it
            c if (0xDC00..=0xDFFF).contains(&c) => {
                result.push_str(&format!("\\u{:04x}", c));
            }
            c => {
                // Regular BMP character
                if let Some(ch) = char::from_u32(c as u32) {
                    result.push(ch);
                }
            }
        }
        i += 1;
    }
    result
}

/// Format a number for JSON output
fn format_number(n: f64) -> String {
    if n.is_nan() || n.is_infinite() {
        return "null".to_string();
    }
    // Check if it's a whole number within safe integer range
    if n.fract() == 0.0 && n.abs() < 9007199254740992.0 {
        format!("{}", n as i64)
    } else {
        format!("{}", n)
    }
}

/// Format an array with optional indentation
fn format_array(items: &[String], indent: &Option<String>, depth: usize) -> String {
    match indent {
        None => format!("[{}]", items.join(",")),
        Some(ind) => {
            let inner_indent = ind.repeat(depth + 1);
            let outer_indent = ind.repeat(depth);
            let formatted_items: Vec<_> = items
                .iter()
                .map(|item| format!("{}{}", inner_indent, item))
                .collect();
            format!("[\n{}\n{}]", formatted_items.join(",\n"), outer_indent)
        }
    }
}

/// Format an object with optional indentation, preserving key order
fn format_object(items: &[(String, String)], indent: &Option<String>, depth: usize) -> String {
    if items.is_empty() {
        return "{}".to_string();
    }

    match indent {
        None => {
            let pairs: Vec<_> = items
                .iter()
                .map(|(k, v)| format!("\"{}\":{}", escape_json_string(k), v))
                .collect();
            format!("{{{}}}", pairs.join(","))
        }
        Some(ind) => {
            let inner_indent = ind.repeat(depth + 1);
            let outer_indent = ind.repeat(depth);
            let pairs: Vec<_> = items
                .iter()
                .map(|(k, v)| format!("{}\"{}\": {}", inner_indent, escape_json_string(k), v))
                .collect();
            format!("{{\n{}\n{}}}", pairs.join(",\n"), outer_indent)
        }
    }
}

/// Call toJSON method on value if it exists
fn call_to_json(value: &Value, key: &str, ncx: &mut NativeContext) -> Result<Value, VmError> {
    // Check if value has toJSON method
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        if let Some(to_json) = obj.get(&PropertyKey::string("toJSON")) {
            if to_json.is_callable() {
                let key_val = Value::string(JsString::intern(key));
                return ncx.call_function(&to_json, value.clone(), &[key_val]);
            }
        }
    }
    // For BigInt, check BigInt.prototype.toJSON
    if value.is_bigint() {
        // Try to get BigInt.prototype from global
        let global = ncx.ctx.global();
        if let Some(bigint_ctor) = global.get(&PropertyKey::string("BigInt")) {
            if let Some(bigint_ctor_obj) = bigint_ctor.as_object() {
                if let Some(bigint_proto) = bigint_ctor_obj.get(&PropertyKey::string("prototype")) {
                    if let Some(proto_obj) = bigint_proto.as_object() {
                        if let Some(to_json) = proto_obj.get(&PropertyKey::string("toJSON")) {
                            if to_json.is_callable() {
                                let key_val = Value::string(JsString::intern(key));
                                return ncx.call_function(&to_json, value.clone(), &[key_val]);
                            }
                        }
                    }
                }
            }
        }
        // BigInt without toJSON should throw TypeError
        return Err(VmError::type_error(
            "Do not know how to serialize a BigInt",
        ));
    }
    Ok(value.clone())
}

/// Call replacer function if present
fn call_replacer(
    replacer_fn: &Option<Value>,
    holder: &Value,
    key: &str,
    value: Value,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    if let Some(replacer) = replacer_fn {
        let key_val = Value::string(JsString::intern(key));
        return ncx.call_function(replacer, holder.clone(), &[key_val, value]);
    }
    Ok(value)
}

/// Get property value from object, properly invoking accessor getters
fn get_property_value(
    obj: &GcRef<JsObject>,
    key: &PropertyKey,
    receiver: &Value,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    if let Some(desc) = obj.lookup_property_descriptor(key) {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if getter.is_callable() {
                        ncx.call_function(&getter, receiver.clone(), &[])
                    } else {
                        Ok(Value::undefined())
                    }
                } else {
                    Ok(Value::undefined())
                }
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Unwrap Number/String/Boolean wrapper objects
fn unwrap_primitive(value: &Value) -> Value {
    if let Some(obj) = value.as_object() {
        // Check for __value__ (Number and Boolean wrapper - both use __value__)
        if let Some(prim) = obj.get(&PropertyKey::string("__value__")) {
            // Could be Number or Boolean
            if prim.as_number().is_some() || prim.as_int32().is_some() || prim.as_boolean().is_some()
            {
                return prim;
            }
        }
        // Check for __primitiveValue__ (String wrapper)
        if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
            if prim.as_string().is_some() {
                return prim;
            }
        }
    }
    value.clone()
}

/// Serialize a value to JSON string (without toJSON/replacer handling)
fn serialize_value_simple(
    value: &Value,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    stack: &mut HashSet<usize>,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    // Depth limit to prevent stack overflow
    if depth > 100 {
        return Ok(Some("null".to_string()));
    }

    // undefined, functions, symbols return None (omitted)
    if value.is_undefined() || value.is_callable() || value.is_symbol() {
        return Ok(None);
    }

    // null
    if value.is_null() {
        return Ok(Some("null".to_string()));
    }

    // BigInt should have been handled by toJSON or should throw
    if value.is_bigint() {
        return Err(VmError::type_error(
            "Do not know how to serialize a BigInt",
        ));
    }

    // Boolean
    if let Some(b) = value.as_boolean() {
        return Ok(Some(if b { "true" } else { "false" }.to_string()));
    }

    // Number (int32 or f64)
    if let Some(n) = value.as_int32() {
        return Ok(Some(format!("{}", n)));
    }
    if let Some(n) = value.as_number() {
        return Ok(Some(format_number(n)));
    }

    // String - use UTF-16 escaping to preserve lone surrogates
    if let Some(s) = value.as_string() {
        return Ok(Some(format!("\"{}\"", escape_json_string_utf16(s.as_utf16()))));
    }

    // Check for array first (both HeapRef::Array and objects with is_array flag)
    let is_array =
        value.as_array().is_some() || value.as_object().map(|o| o.is_array()).unwrap_or(false);

    if is_array {
        return serialize_array_simple(value, indent, property_list, stack, depth, ncx);
    }

    // Regular object
    if value.as_object().is_some() {
        return serialize_object_simple(value, indent, property_list, stack, depth, ncx);
    }

    // Default to null for unknown types
    Ok(Some("null".to_string()))
}

/// Serialize an array (simple version, no toJSON/replacer)
fn serialize_array_simple(
    value: &Value,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    stack: &mut HashSet<usize>,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    let obj = value
        .as_array()
        .or_else(|| value.as_object())
        .ok_or_else(|| VmError::type_error("Expected array"))?;

    // Check for circular reference
    let ptr = obj.as_ptr() as usize;
    if stack.contains(&ptr) {
        return Err(VmError::type_error("Converting circular structure to JSON"));
    }
    stack.insert(ptr);

    let len = obj
        .get(&PropertyKey::string("length"))
        .and_then(|v| {
            v.as_int32()
                .map(|i| i as usize)
                .or_else(|| v.as_number().map(|n| n as usize))
        })
        .unwrap_or(0);

    let mut items = Vec::with_capacity(len);

    for i in 0..len {
        let elem = obj
            .get(&PropertyKey::Index(i as u32))
            .unwrap_or(Value::undefined());
        let elem = unwrap_primitive(&elem);
        match serialize_value_simple(&elem, indent, property_list, stack, depth + 1, ncx)? {
            Some(s) => items.push(s),
            None => items.push("null".to_string()),
        }
    }

    stack.remove(&ptr);
    Ok(Some(format_array(&items, indent, depth)))
}

/// Serialize an object (simple version, no toJSON/replacer)
fn serialize_object_simple(
    value: &Value,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    stack: &mut HashSet<usize>,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    let obj = value
        .as_object()
        .ok_or_else(|| VmError::type_error("Expected object"))?;

    // Check for circular reference
    let ptr = obj.as_ptr() as usize;
    if stack.contains(&ptr) {
        return Err(VmError::type_error("Converting circular structure to JSON"));
    }
    stack.insert(ptr);

    // Get keys - either from property_list (replacer array) or from object
    // Include enumerable own properties (both string keys and integer indices)
    // Per spec, integer indices come first in numeric order, then string keys in insertion order
    let keys: Vec<String> = if let Some(list) = property_list {
        list.clone()
    } else {
        obj.own_keys()
            .into_iter()
            .filter_map(|k| {
                // Check if property is enumerable
                // Note: own_keys() may return Index(i) for properties stored as String("i")
                // so we need to check both forms
                let desc = obj.get_own_property_descriptor(&k).or_else(|| {
                    if let PropertyKey::Index(i) = &k {
                        obj.get_own_property_descriptor(&PropertyKey::string(&i.to_string()))
                    } else {
                        None
                    }
                });
                if let Some(desc) = desc {
                    if desc.enumerable() {
                        return match &k {
                            PropertyKey::String(s) => Some(s.as_str().to_string()),
                            PropertyKey::Index(i) => Some(i.to_string()),
                            PropertyKey::Symbol(_) => None, // Symbols are not included in JSON
                        };
                    }
                }
                None
            })
            .collect()
    };

    let mut items = Vec::new();

    for key in keys {
        if let Some(val) = obj.get(&PropertyKey::string(&key)) {
            let val = unwrap_primitive(&val);
            if let Some(json_val) =
                serialize_value_simple(&val, indent, property_list, stack, depth + 1, ncx)?
            {
                items.push((key, json_val));
            }
        }
    }

    stack.remove(&ptr);
    Ok(Some(format_object(&items, indent, depth)))
}

/// Full stringify with toJSON and replacer support
fn stringify_with_replacer(
    holder: &Value,
    key: &str,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    stack: &mut HashSet<usize>,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    // Depth limit
    if depth > 100 {
        return Ok(Some("null".to_string()));
    }

    // Step 1: Get value from holder (properly invoking getters)
    let value = if let Some(obj) = holder.as_object().or_else(|| holder.as_array()) {
        let prop_key = if let Ok(idx) = key.parse::<u32>() {
            PropertyKey::Index(idx)
        } else {
            PropertyKey::string(key)
        };
        get_property_value(&obj, &prop_key, holder, ncx)?
    } else {
        return Ok(None);
    };

    // Step 2: Call toJSON if present
    let value = call_to_json(&value, key, ncx)?;

    // Step 3: Call replacer function if present
    let value = call_replacer(replacer_fn, holder, key, value, ncx)?;

    // Step 4: Unwrap wrapper objects
    let value = unwrap_primitive(&value);

    // Step 5: Serialize based on type
    // undefined, functions, symbols return None (omitted)
    if value.is_undefined() || value.is_callable() || value.is_symbol() {
        return Ok(None);
    }

    // null
    if value.is_null() {
        return Ok(Some("null".to_string()));
    }

    // BigInt should have been handled by toJSON or should throw
    if value.is_bigint() {
        return Err(VmError::type_error(
            "Do not know how to serialize a BigInt",
        ));
    }

    // Boolean
    if let Some(b) = value.as_boolean() {
        return Ok(Some(if b { "true" } else { "false" }.to_string()));
    }

    // Number (int32 or f64)
    if let Some(n) = value.as_int32() {
        return Ok(Some(format!("{}", n)));
    }
    if let Some(n) = value.as_number() {
        return Ok(Some(format_number(n)));
    }

    // String - use UTF-16 escaping to preserve lone surrogates
    if let Some(s) = value.as_string() {
        return Ok(Some(format!("\"{}\"", escape_json_string_utf16(s.as_utf16()))));
    }

    // Check for array
    let is_array =
        value.as_array().is_some() || value.as_object().map(|o| o.is_array()).unwrap_or(false);

    if is_array {
        return stringify_array_with_replacer(
            &value,
            replacer_fn,
            indent,
            property_list,
            stack,
            depth,
            ncx,
        );
    }

    // Regular object
    if value.as_object().is_some() {
        return stringify_object_with_replacer(
            &value,
            replacer_fn,
            indent,
            property_list,
            stack,
            depth,
            ncx,
        );
    }

    Ok(Some("null".to_string()))
}

/// Stringify array with replacer support
fn stringify_array_with_replacer(
    value: &Value,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    stack: &mut HashSet<usize>,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    let obj = value
        .as_array()
        .or_else(|| value.as_object())
        .ok_or_else(|| VmError::type_error("Expected array"))?;

    let ptr = obj.as_ptr() as usize;
    if stack.contains(&ptr) {
        return Err(VmError::type_error("Converting circular structure to JSON"));
    }
    stack.insert(ptr);

    let len = obj
        .get(&PropertyKey::string("length"))
        .and_then(|v| {
            v.as_int32()
                .map(|i| i as usize)
                .or_else(|| v.as_number().map(|n| n as usize))
        })
        .unwrap_or(0);

    let mut items = Vec::with_capacity(len);

    for i in 0..len {
        let key = i.to_string();
        match stringify_with_replacer(
            value,
            &key,
            replacer_fn,
            indent,
            property_list,
            stack,
            depth + 1,
            ncx,
        )? {
            Some(s) => items.push(s),
            None => items.push("null".to_string()),
        }
    }

    stack.remove(&ptr);
    Ok(Some(format_array(&items, indent, depth)))
}

/// Stringify object with replacer support
fn stringify_object_with_replacer(
    value: &Value,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    stack: &mut HashSet<usize>,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    let obj = value
        .as_object()
        .ok_or_else(|| VmError::type_error("Expected object"))?;

    let ptr = obj.as_ptr() as usize;
    if stack.contains(&ptr) {
        return Err(VmError::type_error("Converting circular structure to JSON"));
    }
    stack.insert(ptr);

    // Get keys - include enumerable own properties (both string keys and integer indices)
    // Per spec, integer indices come first in numeric order, then string keys in insertion order
    let keys: Vec<String> = if let Some(list) = property_list {
        list.clone()
    } else {
        obj.own_keys()
            .into_iter()
            .filter_map(|k| {
                // Check if property is enumerable
                // Note: own_keys() may return Index(i) for properties stored as String("i")
                // so we need to check both forms
                let desc = obj.get_own_property_descriptor(&k).or_else(|| {
                    if let PropertyKey::Index(i) = &k {
                        obj.get_own_property_descriptor(&PropertyKey::string(&i.to_string()))
                    } else {
                        None
                    }
                });
                if let Some(desc) = desc {
                    if desc.enumerable() {
                        return match &k {
                            PropertyKey::String(s) => Some(s.as_str().to_string()),
                            PropertyKey::Index(i) => Some(i.to_string()),
                            PropertyKey::Symbol(_) => None, // Symbols are not included in JSON
                        };
                    }
                }
                None
            })
            .collect()
    };

    let mut items = Vec::new();

    for key in keys {
        if let Some(json_val) = stringify_with_replacer(
            value,
            &key,
            replacer_fn,
            indent,
            property_list,
            stack,
            depth + 1,
            ncx,
        )? {
            items.push((key, json_val));
        }
    }

    stack.remove(&ptr);
    Ok(Some(format_object(&items, indent, depth)))
}

/// Create and install JSON namespace on global object
pub fn install_json_namespace(global: GcRef<JsObject>, mm: &Arc<MemoryManager>) {
    let json_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    // JSON.parse(text, reviver?) — §25.5.1
    let mm_parse = mm.clone();
    let parse_fn = Value::native_function(
        move |_, args, ncx| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());

            // Convert to string using ToString (calling toString() if needed)
            let text = if let Some(s) = arg.as_string() {
                s.as_str().to_string()
            } else if let Some(n) = arg.as_number() {
                format!("{}", n)
            } else if let Some(n) = arg.as_int32() {
                format!("{}", n)
            } else if let Some(b) = arg.as_boolean() {
                if b { "true" } else { "false" }.to_string()
            } else if arg.is_null() {
                "null".to_string()
            } else if arg.is_undefined() {
                return Err(VmError::syntax_error("JSON.parse: unexpected input"));
            } else if let Some(obj) = arg.as_object() {
                // Try calling toString() on the object
                if let Some(to_string_fn) = obj.get(&PropertyKey::string("toString")) {
                    if to_string_fn.is_callable() {
                        let result = ncx.call_function(&to_string_fn, Value::object(obj), &[])?;
                        if let Some(s) = result.as_string() {
                            s.as_str().to_string()
                        } else {
                            return Err(VmError::syntax_error(
                                "JSON.parse: toString did not return string",
                            ));
                        }
                    } else {
                        "[object Object]".to_string()
                    }
                } else {
                    "[object Object]".to_string()
                }
            } else {
                return Err(VmError::syntax_error("JSON.parse: unexpected input"));
            };

            let parsed: serde_json::Value = serde_json::from_str(&text)
                .map_err(|e| VmError::syntax_error(format!("JSON.parse: {}", e)))?;

            // Get Object.prototype and Array.prototype from global
            let global = ncx.ctx.global();
            let object_proto = global
                .get(&PropertyKey::string("Object"))
                .and_then(|o| o.as_object())
                .and_then(|o| o.get(&PropertyKey::string("prototype")))
                .unwrap_or_else(Value::null);
            let array_proto = global
                .get(&PropertyKey::string("Array"))
                .and_then(|o| o.as_object())
                .and_then(|o| o.get(&PropertyKey::string("prototype")))
                .unwrap_or_else(Value::null);

            let result = json_to_value(&parsed, &mm_parse, &object_proto, &array_proto);

            // Apply reviver if provided
            if let Some(reviver) = args.get(1) {
                if reviver.is_callable() {
                    return apply_reviver(result, reviver, ncx, &mm_parse);
                }
            }

            Ok(result)
        },
        mm.clone(),
    );
    if let Some(obj) = parse_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::Data {
                value: Value::int32(2),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::Data {
                value: Value::string(JsString::intern("parse")),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
    }
    json_obj.define_property(
        PropertyKey::string("parse"),
        PropertyDescriptor::builtin_method(parse_fn),
    );

    // JSON.stringify(value, replacer?, space?) — §25.5.2
    let stringify_fn = Value::native_function(
        |_, args, ncx| {
            let val = args.first().cloned().unwrap_or(Value::undefined());

            // undefined at top level returns undefined
            if val.is_undefined() {
                return Ok(Value::undefined());
            }

            // Parse replacer argument
            let (replacer_fn, property_list) = parse_replacer(args.get(1), ncx)?;

            // Parse space argument
            let space_str = parse_space(args.get(2), ncx)?;

            // Create wrapper object to hold the value
            // Per spec, wrapper should have Object.prototype as its prototype
            let global = ncx.ctx.global();
            let object_proto = global
                .get(&PropertyKey::string("Object"))
                .and_then(|o| o.as_object())
                .and_then(|o| o.get(&PropertyKey::string("prototype")))
                .unwrap_or_else(Value::null);
            let wrapper = GcRef::new(JsObject::new(
                object_proto,
                ncx.memory_manager().clone(),
            ));
            wrapper.set(PropertyKey::string(""), val.clone());
            let wrapper_val = Value::object(wrapper);

            let mut stack = HashSet::new();

            // Serialize
            let result = stringify_with_replacer(
                &wrapper_val,
                "",
                &replacer_fn,
                &space_str,
                &property_list,
                &mut stack,
                0,
                ncx,
            )?;

            match result {
                Some(s) => Ok(Value::string(JsString::intern(&s))),
                None => Ok(Value::undefined()),
            }
        },
        mm.clone(),
    );
    if let Some(obj) = stringify_fn.as_object() {
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::Data {
                value: Value::int32(3),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::Data {
                value: Value::string(JsString::intern("stringify")),
                attributes: PropertyAttributes {
                    writable: false,
                    enumerable: false,
                    configurable: true,
                },
            },
        );
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
    }
    json_obj.define_property(
        PropertyKey::string("stringify"),
        PropertyDescriptor::builtin_method(stringify_fn),
    );

    // Set @@toStringTag
    json_obj.define_property(
        PropertyKey::string("@@toStringTag"),
        PropertyDescriptor::Data {
            value: Value::string(JsString::intern("JSON")),
            attributes: PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        },
    );

    global.define_property(
        PropertyKey::string("JSON"),
        PropertyDescriptor::builtin_data(Value::object(json_obj)),
    );
}

/// Parse the replacer argument (can be function or array)
/// Per spec, for objects with [[StringData]] or [[NumberData]], call ToString(v)
fn parse_replacer(
    replacer: Option<&Value>,
    ncx: &mut NativeContext,
) -> Result<(Option<Value>, Option<Vec<String>>), VmError> {
    let Some(r) = replacer else {
        return Ok((None, None));
    };

    if r.is_callable() {
        return Ok((Some(r.clone()), None));
    }

    // Check for array - either HeapRef::Array or object with is_array flag
    let arr = r
        .as_array()
        .or_else(|| r.as_object().filter(|obj| obj.is_array()));
    if let Some(arr) = arr {
        let len = arr
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_int32().or_else(|| v.as_number().map(|n| n as i32)))
            .unwrap_or(0) as usize;

        let mut list = Vec::new();
        let mut seen = HashSet::new();

        for i in 0..len {
            if let Some(item) = arr.get(&PropertyKey::Index(i as u32)) {
                let key = if let Some(s) = item.as_string() {
                    Some(s.as_str().to_string())
                } else if let Some(n) = item.as_int32() {
                    Some(n.to_string())
                } else if let Some(n) = item.as_number() {
                    Some(format_number(n))
                } else if let Some(obj) = item.as_object() {
                    // Check if it's a String or Number wrapper object
                    let is_string_wrapper =
                        obj.get(&PropertyKey::string("__primitiveValue__")).is_some();
                    let is_number_wrapper = obj.get(&PropertyKey::string("__value__")).is_some();

                    if is_string_wrapper || is_number_wrapper {
                        // Per spec: call ToString(v) for both String and Number wrappers
                        if let Some(to_string) = obj.get(&PropertyKey::string("toString")) {
                            if to_string.is_callable() {
                                let result =
                                    ncx.call_function(&to_string, Value::object(obj), &[])?;
                                result.as_string().map(|s| s.as_str().to_string())
                            } else {
                                None
                            }
                        } else {
                            None
                        }
                    } else {
                        None
                    }
                } else {
                    None
                };

                if let Some(k) = key {
                    if !seen.contains(&k) {
                        seen.insert(k.clone());
                        list.push(k);
                    }
                }
            }
        }

        return Ok((None, Some(list)));
    }

    Ok((None, None))
}

/// Parse the space argument
/// Per spec:
/// - If space has [[NumberData]], set space to ToNumber(space)
/// - Else if space has [[StringData]], set space to ToString(space)
fn parse_space(space: Option<&Value>, ncx: &mut NativeContext) -> Result<Option<String>, VmError> {
    let Some(v) = space else {
        return Ok(None);
    };

    // Primitive number
    if let Some(n) = v.as_int32() {
        let n = n.clamp(0, 10) as usize;
        return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
    }
    if let Some(n) = v.as_number() {
        let n = (n.clamp(0.0, 10.0) as i32).max(0) as usize;
        return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
    }

    // Primitive string
    if let Some(s) = v.as_string() {
        let str_val = s.as_str();
        return Ok(if str_val.is_empty() {
            None
        } else {
            Some(str_val.chars().take(10).collect::<String>())
        });
    }

    // Object - check if Number or String wrapper
    if let Some(obj) = v.as_object() {
        // Check for [[NumberData]] - use ToNumber (calls valueOf)
        if obj.get(&PropertyKey::string("__value__")).is_some() {
            // It's a Number wrapper - call valueOf to get the number
            if let Some(value_of) = obj.get(&PropertyKey::string("valueOf")) {
                if value_of.is_callable() {
                    let result = ncx.call_function(&value_of, Value::object(obj.clone()), &[])?;
                    if let Some(n) = result.as_number() {
                        let n = (n.clamp(0.0, 10.0) as i32).max(0) as usize;
                        return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
                    }
                    if let Some(n) = result.as_int32() {
                        let n = n.clamp(0, 10) as usize;
                        return Ok(if n > 0 { Some(" ".repeat(n)) } else { None });
                    }
                }
            }
            return Ok(None);
        }

        // Check for [[StringData]] - use ToString (calls toString)
        if obj.get(&PropertyKey::string("__primitiveValue__")).is_some() {
            // It's a String wrapper - call toString to get the string
            if let Some(to_string) = obj.get(&PropertyKey::string("toString")) {
                if to_string.is_callable() {
                    let result = ncx.call_function(&to_string, Value::object(obj.clone()), &[])?;
                    if let Some(s) = result.as_string() {
                        let str_val = s.as_str();
                        return Ok(if str_val.is_empty() {
                            None
                        } else {
                            Some(str_val.chars().take(10).collect::<String>())
                        });
                    }
                }
            }
            return Ok(None);
        }
    }

    Ok(None)
}

/// Apply reviver function to parsed JSON
fn apply_reviver(
    value: Value,
    reviver: &Value,
    ncx: &mut NativeContext,
    mm: &Arc<MemoryManager>,
) -> Result<Value, VmError> {
    // Create root holder
    let root = GcRef::new(JsObject::new(Value::null(), mm.clone()));
    root.set(PropertyKey::string(""), value.clone());
    let root_val = Value::object(root);

    // Walk and transform
    walk_reviver(&root_val, "", reviver, ncx)
}

/// Recursively apply reviver to parsed value
fn walk_reviver(
    holder: &Value,
    key: &str,
    reviver: &Value,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let value = if let Some(obj) = holder.as_object().or_else(|| holder.as_array()) {
        if let Ok(idx) = key.parse::<u32>() {
            obj.get(&PropertyKey::Index(idx))
                .unwrap_or(Value::undefined())
        } else {
            obj.get(&PropertyKey::string(key))
                .unwrap_or(Value::undefined())
        }
    } else {
        return Ok(Value::undefined());
    };

    // If value is array or object, recurse
    if let Some(arr) = value.as_array() {
        let len = arr
            .get(&PropertyKey::string("length"))
            .and_then(|v| v.as_int32())
            .unwrap_or(0) as usize;

        for i in 0..len {
            let elem_key = i.to_string();
            let new_elem = walk_reviver(&value, &elem_key, reviver, ncx)?;
            if new_elem.is_undefined() {
                // Delete the property
                arr.delete(&PropertyKey::Index(i as u32));
            } else {
                arr.set(PropertyKey::Index(i as u32), new_elem);
            }
        }
    } else if let Some(obj) = value.as_object() {
        let keys = obj.own_keys();
        for k in keys {
            if let PropertyKey::String(s) = &k {
                let new_val = walk_reviver(&value, s.as_str(), reviver, ncx)?;
                if new_val.is_undefined() {
                    obj.delete(&k);
                } else {
                    obj.set(k, new_val);
                }
            }
        }
    }

    // Call reviver
    let key_val = Value::string(JsString::intern(key));
    ncx.call_function(reviver, holder.clone(), &[key_val, value])
}
