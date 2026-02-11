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
use otter_macros::dive;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

/// Tracks visited objects during JSON serialization to detect circular references.
/// Also maintains a path for generating helpful error messages.
struct CircularTracker {
    /// Maps object pointer to index in path
    visited: HashMap<usize, usize>,
    /// Path from root: (property_key, object_ptr, is_array)
    path: Vec<(String, usize, bool)>,
}

impl CircularTracker {
    fn new() -> Self {
        Self {
            visited: HashMap::new(),
            path: Vec::new(),
        }
    }

    /// Try to enter an object. Returns Err with formatted message if circular.
    fn enter(&mut self, key: &str, ptr: usize, is_array: bool) -> Result<(), String> {
        if let Some(&cycle_start) = self.visited.get(&ptr) {
            return Err(self.format_circular_error(key, cycle_start));
        }
        let idx = self.path.len();
        self.visited.insert(ptr, idx);
        self.path.push((key.to_string(), ptr, is_array));
        Ok(())
    }

    /// Exit an object (after serialization)
    fn exit(&mut self, ptr: usize) {
        self.visited.remove(&ptr);
        self.path.pop();
    }

    /// Format a circular reference error message like Node.js/Chrome
    fn format_circular_error(&self, closing_key: &str, cycle_start: usize) -> String {
        let mut msg = String::from("Converting circular structure to JSON");

        if self.path.is_empty() {
            return msg;
        }

        // Get the starting object info
        let (start_key, _, start_is_array) = &self.path[cycle_start];
        let start_type = if *start_is_array { "Array" } else { "Object" };

        if cycle_start == 0 {
            msg.push_str(&format!(
                "\n    --> starting at object with constructor '{}'",
                start_type
            ));
        } else {
            msg.push_str(&format!(
                "\n    --> starting at object with constructor '{}' (property '{}')",
                start_type, start_key
            ));
        }

        // Show intermediate path if there is one
        for i in (cycle_start + 1)..self.path.len() {
            let (key, _, is_array) = &self.path[i];
            let obj_type = if *is_array { "Array" } else { "Object" };
            msg.push_str(&format!(
                "\n    |     property '{}' -> object with constructor '{}'",
                key, obj_type
            ));
        }

        // Show the closing property
        msg.push_str(&format!(
            "\n    --- property '{}' closes the circle",
            closing_key
        ));

        msg
    }
}

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
                let _ = arr.set(
                    PropertyKey::Index(i as u32),
                    json_to_value(item, mm, object_proto, array_proto),
                );
            }
            Value::array(arr)
        }
        serde_json::Value::Object(map) => {
            let obj = GcRef::new(JsObject::new(object_proto.clone(), mm.clone()));
            for (k, v) in map {
                let _ = obj.set(
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
            0x22 => result.push_str("\\\""), // "
            0x5C => result.push_str("\\\\"), // \
            0x0A => result.push_str("\\n"),  // \n
            0x0D => result.push_str("\\r"),  // \r
            0x09 => result.push_str("\\t"),  // \t
            0x08 => result.push_str("\\b"),  // \b
            0x0C => result.push_str("\\f"),  // \f
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

/// Format a number for JSON output (NaN and Infinity become "null")
fn format_number(n: f64) -> String {
    if n.is_nan() || n.is_infinite() {
        return "null".to_string();
    }
    // JSON uses JS Number::toString for number serialization
    crate::globals::js_number_to_string(n)
}

/// Format a number as a property key (JavaScript ToString semantics)
fn number_to_property_key(n: f64) -> String {
    crate::globals::js_number_to_string(n)
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
/// Note: Does NOT throw for BigInt - that's handled after the replacer is called
fn call_to_json(value: &Value, key: &str, ncx: &mut NativeContext) -> Result<Value, VmError> {
    // Check if value has toJSON method
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        // Use get_property_value to properly invoke getter accessors
        let to_json = get_property_value(&obj, &PropertyKey::string("toJSON"), value, ncx)?;
        if to_json.is_callable() {
            let key_val = Value::string(JsString::intern(key));
            return ncx.call_function(&to_json, value.clone(), &[key_val]);
        }
    }
    // For BigInt, check BigInt.prototype.toJSON (but don't throw if not present)
    if value.is_bigint() {
        // Try to get BigInt.prototype from global
        let global = ncx.ctx.global();
        if let Some(bigint_ctor) = global.get(&PropertyKey::string("BigInt")) {
            if let Some(bigint_ctor_obj) = bigint_ctor.as_object() {
                if let Some(bigint_proto) = bigint_ctor_obj.get(&PropertyKey::string("prototype")) {
                    if let Some(proto_obj) = bigint_proto.as_object() {
                        // Use get_property_value to invoke getter accessors
                        // The receiver should be the BigInt value itself for proper `this` binding
                        let to_json = get_property_value(
                            &proto_obj,
                            &PropertyKey::string("toJSON"),
                            value,
                            ncx,
                        )?;
                        if to_json.is_callable() {
                            let key_val = Value::string(JsString::intern(key));
                            return ncx.call_function(&to_json, value.clone(), &[key_val]);
                        }
                    }
                }
            }
        }
        // Don't throw here - let the replacer have a chance first
        // The BigInt error is thrown in stringify_with_replacer after the replacer call
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

/// Check if value is an array (including through proxies)
/// Per ES spec, IsArray recursively unwraps proxies to check the target
fn is_array_value(value: &Value) -> Result<bool, VmError> {
    // Direct array check
    if value.as_array().is_some() {
        return Ok(true);
    }
    // Object with is_array flag
    if let Some(obj) = value.as_object() {
        if obj.is_array() {
            return Ok(true);
        }
    }
    // Proxy: recursively check target
    if let Some(proxy) = value.as_proxy() {
        let target = proxy.target().ok_or_else(|| {
            VmError::type_error("Cannot perform 'IsArray' on a proxy that has been revoked")
        })?;
        return is_array_value(&target);
    }
    Ok(false)
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

/// Convert a value to usize using ToNumber semantics (may throw)
fn value_to_usize(value: &Value, ncx: &mut NativeContext) -> Result<usize, VmError> {
    // Fast path for primitives
    if let Some(n) = value.as_int32() {
        return Ok(n.max(0) as usize);
    }
    if let Some(n) = value.as_number() {
        return Ok((n.max(0.0) as usize).min(usize::MAX));
    }
    // For objects, call valueOf to convert to number
    if let Some(obj) = value.as_object() {
        if let Some(value_of) = obj.get(&PropertyKey::string("valueOf")) {
            if value_of.is_callable() {
                let result = ncx.call_function(&value_of, Value::object(obj.clone()), &[])?;
                if let Some(n) = result.as_int32() {
                    return Ok(n.max(0) as usize);
                }
                if let Some(n) = result.as_number() {
                    return Ok((n.max(0.0) as usize).min(usize::MAX));
                }
            }
        }
    }
    // Default to 0 for other cases
    Ok(0)
}

/// Unwrap Number/String/Boolean wrapper objects (simple version, no function calls)
fn unwrap_primitive(value: &Value) -> Value {
    if let Some(obj) = value.as_object() {
        // Check for __value__ (Number and Boolean wrapper - both use __value__)
        if let Some(prim) = obj.get(&PropertyKey::string("__value__")) {
            // Could be Number or Boolean
            if prim.as_number().is_some()
                || prim.as_int32().is_some()
                || prim.as_boolean().is_some()
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

/// Unwrap wrapper objects per ES spec - calls ToString for String wrappers, ToNumber for Number wrappers
fn unwrap_primitive_with_calls(value: &Value, ncx: &mut NativeContext) -> Result<Value, VmError> {
    if let Some(obj) = value.as_object() {
        // Check for [[StringData]] (String wrapper) - call ToString
        if obj
            .get(&PropertyKey::string("__primitiveValue__"))
            .is_some()
        {
            if let Some(to_string) = obj.get(&PropertyKey::string("toString")) {
                if to_string.is_callable() {
                    return ncx.call_function(&to_string, value.clone(), &[]);
                }
            }
            // Fallback to primitive value
            if let Some(prim) = obj.get(&PropertyKey::string("__primitiveValue__")) {
                if prim.as_string().is_some() {
                    return Ok(prim);
                }
            }
        }
        // Check for [[NumberData]] (Number wrapper) - call valueOf (ToNumber)
        if let Some(prim) = obj.get(&PropertyKey::string("__value__")) {
            if prim.as_number().is_some() || prim.as_int32().is_some() {
                // Call valueOf to get the number
                if let Some(value_of) = obj.get(&PropertyKey::string("valueOf")) {
                    if value_of.is_callable() {
                        return ncx.call_function(&value_of, value.clone(), &[]);
                    }
                }
                // Fallback to primitive value
                return Ok(prim);
            }
        }
        // Check for [[BooleanData]] (Boolean wrapper)
        if let Some(prim) = obj.get(&PropertyKey::string("__value__")) {
            if prim.as_boolean().is_some() {
                return Ok(prim);
            }
        }
    }
    Ok(value.clone())
}

/// Serialize a value to JSON string (without toJSON/replacer handling)
fn serialize_value_simple(
    value: &Value,
    key: &str,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
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
        return Err(VmError::type_error("Do not know how to serialize a BigInt"));
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
        return Ok(Some(format!(
            "\"{}\"",
            escape_json_string_utf16(s.as_utf16())
        )));
    }

    // Check for array (including proxy arrays)
    if is_array_value(value)? {
        return serialize_array_simple(value, key, indent, property_list, tracker, depth, ncx);
    }

    // Regular object
    if value.as_object().is_some() {
        return serialize_object_simple(value, key, indent, property_list, tracker, depth, ncx);
    }

    // Default to null for unknown types
    Ok(Some("null".to_string()))
}

/// Serialize an array (simple version, no toJSON/replacer)
fn serialize_array_simple(
    value: &Value,
    key: &str,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    let obj = value
        .as_array()
        .or_else(|| value.as_object())
        .ok_or_else(|| VmError::type_error("Expected array"))?;

    // Check for circular reference
    let ptr = obj.as_ptr() as usize;
    if let Err(msg) = tracker.enter(key, ptr, true) {
        return Err(VmError::type_error(msg));
    }

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
        let elem_key = i.to_string();
        match serialize_value_simple(
            &elem,
            &elem_key,
            indent,
            property_list,
            tracker,
            depth + 1,
            ncx,
        )? {
            Some(s) => items.push(s),
            None => items.push("null".to_string()),
        }
    }

    tracker.exit(ptr);
    Ok(Some(format_array(&items, indent, depth)))
}

/// Serialize an object (simple version, no toJSON/replacer)
fn serialize_object_simple(
    value: &Value,
    obj_key: &str,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    let obj = value
        .as_object()
        .ok_or_else(|| VmError::type_error("Expected object"))?;

    // Check for circular reference
    let ptr = obj.as_ptr() as usize;
    if let Err(msg) = tracker.enter(obj_key, ptr, false) {
        return Err(VmError::type_error(msg));
    }

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
                serialize_value_simple(&val, &key, indent, property_list, tracker, depth + 1, ncx)?
            {
                items.push((key, json_val));
            }
        }
    }

    tracker.exit(ptr);
    Ok(Some(format_object(&items, indent, depth)))
}

/// Full stringify with toJSON and replacer support
fn stringify_with_replacer(
    holder: &Value,
    key: &str,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    // Depth limit
    if depth > 100 {
        return Ok(Some("null".to_string()));
    }

    // Step 1: Get value from holder (properly invoking getters)
    let (prop_key, key_value) = if let Ok(idx) = key.parse::<u32>() {
        // For numeric keys, pass as number so Reflect.get works correctly with arrays
        (PropertyKey::Index(idx), Value::int32(idx as i32))
    } else {
        (
            PropertyKey::string(key),
            Value::string(JsString::intern(key)),
        )
    };
    let value = if let Some(obj) = holder.as_object().or_else(|| holder.as_array()) {
        get_property_value(&obj, &prop_key, holder, ncx)?
    } else if let Some(proxy) = holder.as_proxy() {
        // For proxies, invoke the get trap
        crate::proxy_operations::proxy_get(ncx, proxy, &prop_key, key_value, holder.clone())?
    } else {
        return Ok(None);
    };

    // Step 2: Call toJSON if present
    let value = call_to_json(&value, key, ncx)?;

    // Step 3: Call replacer function if present
    let value = call_replacer(replacer_fn, holder, key, value, ncx)?;

    // Step 4: Unwrap wrapper objects (calls ToString for String wrappers per spec)
    let value = unwrap_primitive_with_calls(&value, ncx)?;

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
        return Err(VmError::type_error("Do not know how to serialize a BigInt"));
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
        return Ok(Some(format!(
            "\"{}\"",
            escape_json_string_utf16(s.as_utf16())
        )));
    }

    // Check for array (including proxy arrays)
    if is_array_value(&value)? {
        return stringify_array_with_replacer(
            &value,
            key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth,
            ncx,
        );
    }

    // Regular object or proxy
    if value.as_object().is_some() || value.as_proxy().is_some() {
        return stringify_object_with_replacer(
            &value,
            key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth,
            ncx,
        );
    }

    Ok(Some("null".to_string()))
}

/// Stringify array with replacer support
fn stringify_array_with_replacer(
    value: &Value,
    arr_key: &str,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    // Get pointer for circular reference checking
    // Works for arrays, objects, and proxies
    let ptr = if let Some(obj) = value.as_array().or_else(|| value.as_object()) {
        obj.as_ptr() as usize
    } else if let Some(proxy) = value.as_proxy() {
        proxy.as_ptr() as usize
    } else {
        return Err(VmError::type_error("Expected array"));
    };

    if let Err(msg) = tracker.enter(arr_key, ptr, true) {
        return Err(VmError::type_error(msg));
    }

    // Get length - use property access that works for both objects and proxies
    let length_key = PropertyKey::string("length");
    let length_val = if let Some(obj) = value.as_array().or_else(|| value.as_object()) {
        obj.get(&length_key).unwrap_or(Value::int32(0))
    } else if let Some(proxy) = value.as_proxy() {
        // For proxies, invoke the get trap
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &length_key,
            Value::string(JsString::intern("length")),
            value.clone(),
        )?
    } else {
        Value::int32(0)
    };

    // Convert length to number using ToNumber semantics (may throw)
    let len = value_to_usize(&length_val, ncx)?;

    let mut items = Vec::with_capacity(len);

    for i in 0..len {
        let key = i.to_string();
        match stringify_with_replacer(
            value,
            &key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth + 1,
            ncx,
        )? {
            Some(s) => items.push(s),
            None => items.push("null".to_string()),
        }
    }

    tracker.exit(ptr);
    Ok(Some(format_array(&items, indent, depth)))
}

/// Stringify object with replacer support
fn stringify_object_with_replacer(
    value: &Value,
    obj_key: &str,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
) -> Result<Option<String>, VmError> {
    // Get pointer for circular reference checking - works for objects and proxies
    let (ptr, keys) = if let Some(obj) = value.as_object() {
        let ptr = obj.as_ptr() as usize;
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
        (ptr, keys)
    } else if let Some(proxy) = value.as_proxy() {
        let ptr = proxy.as_ptr() as usize;
        // For proxies, use the property list if available, otherwise use proxy ownKeys trap
        let keys: Vec<String> = if let Some(list) = property_list {
            list.clone()
        } else {
            // Get keys from proxy using ownKeys trap
            let proxy_keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
            proxy_keys
                .into_iter()
                .filter_map(|k| match k {
                    PropertyKey::String(s) => Some(s.as_str().to_string()),
                    PropertyKey::Index(i) => Some(i.to_string()),
                    PropertyKey::Symbol(_) => None,
                })
                .collect()
        };
        (ptr, keys)
    } else {
        return Err(VmError::type_error("Expected object"));
    };

    if let Err(msg) = tracker.enter(obj_key, ptr, false) {
        return Err(VmError::type_error(msg));
    }

    let mut items = Vec::new();

    for key in keys {
        if let Some(json_val) = stringify_with_replacer(
            value,
            &key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth + 1,
            ncx,
        )? {
            items.push((key, json_val));
        }
    }

    tracker.exit(ptr);
    Ok(Some(format_object(&items, indent, depth)))
}

#[dive(name = "parse", length = 2)]
fn json_parse(
    _this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
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

    let mm = ncx.memory_manager().clone();
    let result = json_to_value(&parsed, &mm, &object_proto, &array_proto);

    // Apply reviver if provided
    if let Some(reviver) = args.get(1) {
        if reviver.is_callable() {
            return apply_reviver(result, reviver, ncx, &mm);
        }
    }

    Ok(result)
}

#[dive(name = "stringify", length = 3)]
fn json_stringify(
    _this_val: &Value,
    args: &[Value],
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
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
    let wrapper = GcRef::new(JsObject::new(object_proto, ncx.memory_manager().clone()));
    let _ = wrapper.set(PropertyKey::string(""), val.clone());
    let wrapper_val = Value::object(wrapper);

    let mut tracker = CircularTracker::new();

    // Serialize
    let result = stringify_with_replacer(
        &wrapper_val,
        "",
        &replacer_fn,
        &space_str,
        &property_list,
        &mut tracker,
        0,
        ncx,
    )?;

    match result {
        Some(s) => Ok(Value::string(JsString::intern(&s))),
        None => Ok(Value::undefined()),
    }
}

/// Create and install JSON namespace on global object
pub fn install_json_namespace(
    global: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    function_prototype: GcRef<JsObject>,
) {
    let json_obj = GcRef::new(JsObject::new(Value::null(), mm.clone()));

    // JSON.parse(text, reviver?) — §25.5.1
    let (parse_name, parse_native, parse_length) = json_parse_decl();
    let parse_fn = Value::native_function_from_arc(parse_native, mm.clone());
    if let Some(obj) = parse_fn.as_object() {
        // Set Function.prototype as prototype
        obj.set_prototype(Value::object(function_prototype));
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::Data {
                value: Value::int32(parse_length as i32),
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
                value: Value::string(JsString::intern(parse_name)),
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
    let (stringify_name, stringify_native, stringify_length) = json_stringify_decl();
    let stringify_fn = Value::native_function_from_arc(stringify_native, mm.clone());
    if let Some(obj) = stringify_fn.as_object() {
        // Set Function.prototype as prototype
        obj.set_prototype(Value::object(function_prototype));
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::Data {
                value: Value::int32(stringify_length as i32),
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
                value: Value::string(JsString::intern(stringify_name)),
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

/// Get a property value from replacer (array or proxy), invoking accessor getters and proxy traps
fn get_replacer_element(
    replacer: &Value,
    index: u32,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    let key = PropertyKey::Index(index);
    // Also try string key for accessor properties defined with string "0", "1", etc.
    let str_key = PropertyKey::String(JsString::intern(&index.to_string()));

    // For arrays/objects
    if let Some(obj) = replacer.as_array().or_else(|| replacer.as_object()) {
        // First check if there's an accessor property (could be defined on string key)
        if let Some(PropertyDescriptor::Accessor { get, .. }) =
            obj.get_own_property_descriptor(&str_key)
        {
            if let Some(getter) = get {
                if getter.is_callable() {
                    return ncx.call_function(&getter, replacer.clone(), &[]);
                }
            }
            return Ok(Value::undefined());
        }
        // Otherwise use direct access
        return Ok(obj.get(&key).unwrap_or(Value::undefined()));
    }

    // For proxies, use proxy_get to invoke the get trap
    if let Some(proxy) = replacer.as_proxy() {
        return crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &key,
            Value::int32(index as i32),
            replacer.clone(),
        );
    }

    Ok(Value::undefined())
}

/// Get length from replacer (array or proxy), converting to number (may throw)
fn get_replacer_length(replacer: &Value, ncx: &mut NativeContext) -> Result<usize, VmError> {
    let length_key = PropertyKey::string("length");

    // Get the length value - either from object or proxy
    let len_val = if let Some(obj) = replacer.as_array().or_else(|| replacer.as_object()) {
        // Use obj.get() which has special handling for array "length"
        obj.get(&length_key).unwrap_or(Value::int32(0))
    } else if let Some(proxy) = replacer.as_proxy() {
        crate::proxy_operations::proxy_get(
            ncx,
            proxy,
            &length_key,
            Value::string(JsString::intern("length")),
            replacer.clone(),
        )?
    } else {
        return Ok(0);
    };

    // Convert to number using ToNumber semantics (may throw for objects with throwing valueOf)
    value_to_usize(&len_val, ncx)
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

    // Check for array - use is_array_value to handle proxies wrapping arrays
    if is_array_value(r)? {
        let len = get_replacer_length(r, ncx)?;

        let mut list = Vec::new();
        let mut seen = HashSet::new();

        for i in 0..len {
            let item = get_replacer_element(r, i as u32, ncx)?;

            let key = if let Some(s) = item.as_string() {
                Some(s.as_str().to_string())
            } else if let Some(n) = item.as_int32() {
                Some(n.to_string())
            } else if let Some(n) = item.as_number() {
                // Use JavaScript ToString semantics for property keys (preserves NaN, Infinity)
                Some(number_to_property_key(n))
            } else if let Some(obj) = item.as_object() {
                // Check if it's a String or Number wrapper object
                let is_string_wrapper = obj
                    .get(&PropertyKey::string("__primitiveValue__"))
                    .is_some();
                let is_number_wrapper = obj.get(&PropertyKey::string("__value__")).is_some();

                if is_string_wrapper || is_number_wrapper {
                    // Per spec: call ToString(v) for both String and Number wrappers
                    if let Some(to_string) = obj.get(&PropertyKey::string("toString")) {
                        if to_string.is_callable() {
                            let result = ncx.call_function(&to_string, Value::object(obj), &[])?;
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
        if obj
            .get(&PropertyKey::string("__primitiveValue__"))
            .is_some()
        {
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
    let _ = root.set(PropertyKey::string(""), value.clone());
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
    // Get value from holder
    let value = get_reviver_value(holder, key, ncx)?;

    // If value is array, recurse into array elements
    if is_array_for_reviver(&value, ncx)? {
        let len = get_length_for_reviver(&value, ncx)?;

        for i in 0..len {
            let elem_key = i.to_string();
            let new_elem = walk_reviver(&value, &elem_key, reviver, ncx)?;
            let prop_key = PropertyKey::Index(i as u32);
            let key_val = Value::string(JsString::intern(&elem_key));
            if new_elem.is_undefined() {
                // Delete the property
                delete_reviver_property(&value, &prop_key, key_val, ncx)?;
            } else {
                // CreateDataProperty - triggers proxy defineProperty trap
                create_data_property(&value, &prop_key, key_val, new_elem, ncx)?;
            }
        }
    } else if is_object_for_reviver(&value) {
        // Get enumerable own property keys
        let keys = get_enumerable_keys(&value, ncx)?;
        for key_str in keys {
            let new_val = walk_reviver(&value, &key_str, reviver, ncx)?;
            let prop_key = PropertyKey::string(&key_str);
            let key_val = Value::string(JsString::intern(&key_str));
            if new_val.is_undefined() {
                delete_reviver_property(&value, &prop_key, key_val, ncx)?;
            } else {
                // CreateDataProperty - triggers proxy defineProperty trap
                create_data_property(&value, &prop_key, key_val, new_val, ncx)?;
            }
        }
    }

    // Call reviver
    let key_val = Value::string(JsString::intern(key));
    ncx.call_function(reviver, holder.clone(), &[key_val, value])
}

/// Get value from holder during reviver walk
fn get_reviver_value(holder: &Value, key: &str, ncx: &mut NativeContext) -> Result<Value, VmError> {
    let prop_key = if let Ok(idx) = key.parse::<u32>() {
        PropertyKey::Index(idx)
    } else {
        PropertyKey::string(key)
    };
    let key_val = Value::string(JsString::intern(key));

    if let Some(proxy) = holder.as_proxy() {
        crate::proxy_operations::proxy_get(ncx, proxy, &prop_key, key_val, holder.clone())
    } else if let Some(obj) = holder.as_object().or_else(|| holder.as_array()) {
        Ok(obj.get(&prop_key).unwrap_or(Value::undefined()))
    } else {
        Ok(Value::undefined())
    }
}

/// Check if value is an array (handles proxies)
fn is_array_for_reviver(value: &Value, ncx: &mut NativeContext) -> Result<bool, VmError> {
    if let Some(proxy) = value.as_proxy() {
        let target = proxy
            .target()
            .ok_or_else(|| VmError::type_error("Cannot check isArray on revoked proxy"))?;
        return is_array_for_reviver(&target, ncx);
    }
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        return Ok(obj.is_array());
    }
    Ok(false)
}

/// Check if value is an object (not array, handles proxies)
fn is_object_for_reviver(value: &Value) -> bool {
    if let Some(proxy) = value.as_proxy() {
        if let Some(target) = proxy.target() {
            return is_object_for_reviver(&target);
        }
        return false;
    }
    if value.as_array().is_some() {
        return false;
    }
    value.as_object().is_some()
}

/// Get length for array during reviver walk
fn get_length_for_reviver(value: &Value, ncx: &mut NativeContext) -> Result<usize, VmError> {
    let length_key = PropertyKey::string("length");
    let key_val = Value::string(JsString::intern("length"));

    let len_val = if let Some(proxy) = value.as_proxy() {
        crate::proxy_operations::proxy_get(ncx, proxy, &length_key, key_val, value.clone())?
    } else if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        obj.get(&length_key).unwrap_or(Value::int32(0))
    } else {
        return Ok(0);
    };

    value_to_usize(&len_val, ncx)
}

/// Get enumerable own property keys from object/proxy
fn get_enumerable_keys(value: &Value, ncx: &mut NativeContext) -> Result<Vec<String>, VmError> {
    // Helper to convert PropertyKey to string representation
    fn key_to_string(k: PropertyKey) -> String {
        match k {
            PropertyKey::String(s) => s.as_str().to_string(),
            PropertyKey::Index(i) => i.to_string(),
            PropertyKey::Symbol(_) => String::new(), // Symbols not included in enumerable keys
        }
    }

    if let Some(proxy) = value.as_proxy() {
        // proxy_own_keys returns Vec<PropertyKey>
        let keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
        return Ok(keys
            .into_iter()
            .map(key_to_string)
            .filter(|s| !s.is_empty())
            .collect());
    }
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        let keys = obj.own_keys();
        return Ok(keys
            .into_iter()
            .map(key_to_string)
            .filter(|s| !s.is_empty())
            .collect());
    }
    Ok(Vec::new())
}

/// Delete property during reviver walk (handles proxies)
fn delete_reviver_property(
    value: &Value,
    key: &PropertyKey,
    key_val: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    if let Some(proxy) = value.as_proxy() {
        crate::proxy_operations::proxy_delete_property(ncx, proxy, key, key_val)?;
    } else if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        obj.delete(key);
    }
    Ok(())
}

/// CreateDataProperty - creates data property, triggering proxy traps if applicable
/// Per spec, this fails silently for non-configurable properties
fn create_data_property(
    value: &Value,
    key: &PropertyKey,
    key_val: Value,
    new_value: Value,
    ncx: &mut NativeContext,
) -> Result<(), VmError> {
    if let Some(proxy) = value.as_proxy() {
        // Use defineProperty trap to create data property
        let desc = PropertyDescriptor::Data {
            value: new_value,
            attributes: PropertyAttributes {
                writable: true,
                enumerable: true,
                configurable: true,
            },
        };
        crate::proxy_operations::proxy_define_property(ncx, proxy, key, key_val, &desc)?;
    } else if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        // Check if property is non-configurable - if so, CreateDataProperty fails silently
        // We need to check both Index key and String key forms for array indices
        let existing_desc = obj.get_own_property_descriptor(key);

        if let Some(desc) = existing_desc {
            if !desc.is_configurable() {
                // Cannot redefine non-configurable property - fail silently
                return Ok(());
            }
        }

        let _ = obj.set(key.clone(), new_value);
    }
    Ok(())
}
