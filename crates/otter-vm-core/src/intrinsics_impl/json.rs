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
use rustc_hash::{FxHashMap, FxHashSet};
use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};
use std::sync::Arc;

/// Native JSON hot-loop interrupt check cadence (power-of-two for bitmask checks).
const JSON_INTERRUPT_CHECK_INTERVAL: usize = 1024;

#[inline]
fn maybe_check_interrupt(ncx: &mut NativeContext<'_>, index: usize) -> Result<(), VmError> {
    if (index & (JSON_INTERRUPT_CHECK_INTERVAL - 1)) == 0 {
        ncx.check_for_interrupt()?;
    }
    Ok(())
}

/// Tracks visited objects during JSON serialization to detect circular references.
/// Also maintains a path for generating helpful error messages.
struct CircularTracker {
    /// Maps object pointer to index in path
    visited: FxHashMap<usize, usize>,
    /// Path from root: (property_key, object_ptr, is_array)
    path: Vec<(String, usize, bool)>,
}

impl CircularTracker {
    fn new() -> Self {
        Self {
            visited: FxHashMap::default(),
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

struct JsonParseState<'a, 'ctx> {
    mm: &'a Arc<MemoryManager>,
    object_proto: &'a Value,
    array_proto: &'a Value,
    key_cache: &'a mut FxHashMap<String, GcRef<JsString>>,
    node_count: usize,
    ncx: &'a mut NativeContext<'ctx>,
}

impl<'a, 'ctx> JsonParseState<'a, 'ctx> {
    #[inline]
    fn before_node(&mut self) -> Result<(), VmError> {
        self.node_count += 1;
        maybe_check_interrupt(self.ncx, self.node_count)
    }
}

struct JsonValueSeed<'s, 'a, 'ctx> {
    state: &'s mut JsonParseState<'a, 'ctx>,
}

struct JsonValueVisitor<'s, 'a, 'ctx> {
    state: &'s mut JsonParseState<'a, 'ctx>,
}

impl<'de, 's, 'a, 'ctx> DeserializeSeed<'de> for JsonValueSeed<'s, 'a, 'ctx> {
    type Value = Value;

    fn deserialize<D>(self, deserializer: D) -> Result<Self::Value, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        self.state
            .before_node()
            .map_err(|e| de::Error::custom(e.to_string()))?;
        deserializer.deserialize_any(JsonValueVisitor { state: self.state })
    }
}

impl<'de, 's, 'a, 'ctx> Visitor<'de> for JsonValueVisitor<'s, 'a, 'ctx> {
    type Value = Value;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("a valid JSON value")
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::null())
    }

    fn visit_bool<E>(self, v: bool) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::boolean(v))
    }

    fn visit_i64<E>(self, v: i64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if let Ok(i) = i32::try_from(v) {
            Ok(Value::int32(i))
        } else {
            Ok(Value::number(v as f64))
        }
    }

    fn visit_u64<E>(self, v: u64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        if let Ok(i) = i32::try_from(v) {
            Ok(Value::int32(i))
        } else {
            Ok(Value::number(v as f64))
        }
    }

    fn visit_f64<E>(self, v: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::number(v))
    }

    fn visit_str<E>(self, v: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::string(JsString::intern(v)))
    }

    fn visit_borrowed_str<E>(self, v: &'de str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        self.visit_str(v)
    }

    fn visit_string<E>(self, v: String) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        Ok(Value::string(JsString::intern(&v)))
    }

    fn visit_seq<A>(self, mut seq: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let hint = seq.size_hint().unwrap_or(0);
        let arr = GcRef::new(JsObject::array(hint));
        arr.set_prototype(self.state.array_proto.clone());

        let mut index = 0usize;
        while let Some(value) = seq.next_element_seed(JsonValueSeed { state: self.state })? {
            arr.initialize_array_element(index, value);
            index += 1;
        }

        Ok(Value::array(arr))
    }

    fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let obj = GcRef::new(JsObject::new(
            self.state.object_proto.clone(),
        ));

        let mut seen_keys = FxHashSet::default();
        let mut index = 0usize;

        while let Some(raw_key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            maybe_check_interrupt(self.state.ncx, index)
                .map_err(|e| de::Error::custom(e.to_string()))?;
            index += 1;

            let key = if let Some(cached) = self.state.key_cache.get(raw_key.as_ref()) {
                *cached
            } else {
                let new_key = JsString::intern(raw_key.as_ref());
                self.state.key_cache.insert(raw_key.into_owned(), new_key);
                new_key
            };

            let value = map.next_value_seed(JsonValueSeed { state: self.state })?;
            if seen_keys.insert(key) {
                obj.define_data_property_for_construction(key, value);
            } else {
                obj.set(PropertyKey::String(key), value)
                    .map_err(|e| de::Error::custom(e.to_string()))?;
            }
        }

        Ok(Value::object(obj))
    }
}

fn parse_json_to_value_direct(
    text: &str,
    mm: &Arc<MemoryManager>,
    object_proto: &Value,
    array_proto: &Value,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    let mut key_cache = FxHashMap::default();
    let mut state = JsonParseState {
        mm,
        object_proto,
        array_proto,
        key_cache: &mut key_cache,
        node_count: 0,
        ncx,
    };

    let mut deserializer = serde_json::Deserializer::from_str(text);
    let value = JsonValueSeed { state: &mut state }
        .deserialize(&mut deserializer)
        .map_err(|e| VmError::syntax_error(format!("JSON.parse: {e}")))?;

    deserializer
        .end()
        .map_err(|e| VmError::syntax_error(format!("JSON.parse: {e}")))?;

    Ok(value)
}

use std::fmt::Write; // Needed for write! on String

/// Escape special characters in JSON strings (using UTF-8 input)
fn escape_json_string(s: &str, out: &mut String) {
    for c in s.chars() {
        let code = c as u32;
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\x08' => out.push_str("\\b"),
            '\x0C' => out.push_str("\\f"),
            c if code < 0x20 => {
                let _ = write!(out, "\\u{:04x}", code);
            }
            c => out.push(c),
        }
    }
}

/// Escape JSON string preserving lone surrogates from UTF-16 data
fn escape_json_string_utf16(units: &[u16], out: &mut String) {
    let mut i = 0;
    while i < units.len() {
        let code = units[i];
        match code {
            0x22 => out.push_str("\\\""), // "
            0x5C => out.push_str("\\\\"), // \
            0x0A => out.push_str("\\n"),  // \n
            0x0D => out.push_str("\\r"),  // \r
            0x09 => out.push_str("\\t"),  // \t
            0x08 => out.push_str("\\b"),  // \b
            0x0C => out.push_str("\\f"),  // \f
            c if c < 0x20 => {
                let _ = write!(out, "\\u{:04x}", c);
            }
            // High surrogate
            c if (0xD800..=0xDBFF).contains(&c) => {
                // Check for valid surrogate pair
                if i + 1 < units.len() && (0xDC00..=0xDFFF).contains(&units[i + 1]) {
                    // Valid pair - decode to code point and output as UTF-8
                    let high = (c as u32 - 0xD800) << 10;
                    let low = units[i + 1] as u32 - 0xDC00;
                    let cp = 0x10000 + high + low;
                    if let Some(ch) = char::from_u32(cp) {
                        out.push(ch);
                    }
                    i += 1; // Skip the low surrogate
                } else {
                    // Lone high surrogate - escape it
                    let _ = write!(out, "\\u{:04x}", c);
                }
            }
            // Lone low surrogate - escape it
            c if (0xDC00..=0xDFFF).contains(&c) => {
                let _ = write!(out, "\\u{:04x}", c);
            }
            c => {
                // Regular BMP character
                if let Some(ch) = char::from_u32(c as u32) {
                    out.push(ch);
                }
            }
        }
        i += 1;
    }
}

/// Format a number for JSON output (NaN and Infinity become "null")
fn format_number(n: f64, out: &mut String) {
    if n.is_nan() || n.is_infinite() {
        out.push_str("null");
    } else {
        out.push_str(&crate::globals::js_number_to_string(n));
    }
}

/// Format a number as a property key (JavaScript ToString semantics)
fn number_to_property_key(n: f64) -> String {
    crate::globals::js_number_to_string(n)
}

#[inline]
fn stringify_callback_key_value(prop_key: PropertyKey, key_text: &str) -> Value {
    match prop_key {
        PropertyKey::String(s) => Value::string(s),
        PropertyKey::Index(_) => Value::string(JsString::intern(key_text)),
        PropertyKey::Symbol(_) => Value::undefined(),
    }
}

#[inline]
fn stringify_access_key_value_for_proxy(prop_key: PropertyKey, key_text: &str) -> Value {
    match prop_key {
        PropertyKey::Index(i) if i <= i32::MAX as u32 => Value::int32(i as i32),
        PropertyKey::Index(i) => Value::number(i as f64),
        PropertyKey::String(_) => stringify_callback_key_value(prop_key, key_text),
        PropertyKey::Symbol(_) => Value::undefined(),
    }
}

/// Call toJSON method on value if it exists
/// Note: Does NOT throw for BigInt - that's handled after the replacer is called
fn call_to_json(
    value: &Value,
    key_text: &str,
    prop_key: PropertyKey,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    // Check if value has toJSON method
    if let Some(obj) = value.as_object().or_else(|| value.as_array()) {
        // Use get_property_value to properly invoke getter accessors
        let to_json = get_property_value(&obj, &PropertyKey::string("toJSON"), value, ncx)?;
        if to_json.is_callable() {
            let key_value = stringify_callback_key_value(prop_key, key_text);
            return ncx.call_function(&to_json, value.clone(), &[key_value]);
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
                            let key_value = stringify_callback_key_value(prop_key, key_text);
                            return ncx.call_function(&to_json, value.clone(), &[key_value]);
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
    key_text: &str,
    prop_key: PropertyKey,
    value: Value,
    ncx: &mut NativeContext,
) -> Result<Value, VmError> {
    if let Some(replacer) = replacer_fn {
        let key_value = stringify_callback_key_value(prop_key, key_text);
        return ncx.call_function(replacer, holder.clone(), &[key_value, value]);
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
    // Common fast path: ordinary data properties (own/prototype chain) without accessors.
    // JsObject::get already handles prototype traversal for data properties.
    if let Some(value) = obj.get(key) {
        return Ok(value);
    }

    // Slow path: accessor descriptors (where JsObject::get intentionally returns None).
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
    out: &mut String,
) -> Result<bool, VmError> {
    // Depth limit to prevent stack overflow
    if depth > 100 {
        out.push_str("null");
        return Ok(true);
    }

    // undefined, functions, symbols return false (omitted)
    if value.is_undefined() || value.is_callable() || value.is_symbol() {
        return Ok(false);
    }

    // null
    if value.is_null() {
        out.push_str("null");
        return Ok(true);
    }

    // BigInt should have been handled by toJSON or should throw
    if value.is_bigint() {
        return Err(VmError::type_error("Do not know how to serialize a BigInt"));
    }

    // Boolean
    if let Some(b) = value.as_boolean() {
        out.push_str(if b { "true" } else { "false" });
        return Ok(true);
    }

    // Number (int32 or f64)
    if let Some(n) = value.as_int32() {
        let _ = write!(out, "{}", n);
        return Ok(true);
    }
    if let Some(n) = value.as_number() {
        format_number(n, out);
        return Ok(true);
    }

    // String - use UTF-16 escaping to preserve lone surrogates
    if let Some(s) = value.as_string() {
        out.push('"');
        escape_json_string_utf16(s.as_utf16(), out);
        out.push('"');
        return Ok(true);
    }

    // Check for array (including proxy arrays)
    if is_array_value(value)? {
        serialize_array_simple(value, key, indent, property_list, tracker, depth, ncx, out)?;
        return Ok(true);
    }

    // Regular object
    if value.as_object().is_some() {
        serialize_object_simple(value, key, indent, property_list, tracker, depth, ncx, out)?;
        return Ok(true);
    }

    // Default to null for unknown types
    out.push_str("null");
    Ok(true)
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
    out: &mut String,
) -> Result<(), VmError> {
    let obj = value
        .as_array()
        .or_else(|| value.as_object())
        .ok_or_else(|| VmError::type_error("Expected array"))?;

    // Check for circular reference
    let ptr = obj.as_ptr() as usize;
    if let Err(msg) = tracker.enter(key, ptr, true) {
        return Err(VmError::type_error(msg));
    }

    let len = if obj.is_array() {
        obj.array_length()
    } else {
        obj.get(&PropertyKey::string("length"))
            .and_then(|v| {
                v.as_int32()
                    .map(|i| i as usize)
                    .or_else(|| v.as_number().map(|n| n as usize))
            })
            .unwrap_or(0)
    };

    if len == 0 {
        out.push_str("[]");
        tracker.exit(ptr);
        return Ok(());
    }

    out.push('[');
    if let Some(ind) = indent {
        out.push('\n');
    }

    for i in 0..len {
        maybe_check_interrupt(ncx, i)?;
        if i > 0 {
            out.push(',');
            if let Some(ind) = indent {
                out.push('\n');
            }
        }
        if let Some(ind) = indent {
            for _ in 0..=depth {
                out.push_str(ind);
            }
        }

        // Use direct elements access for arrays to avoid full property lookup
        let elem = {
            let elements = obj.elements.borrow();
            if i < elements.len() {
                let v = &elements[i];
                if !v.is_hole() { v.clone() } else { Value::undefined() }
            } else {
                Value::undefined()
            }
        };
        let elem = unwrap_primitive(&elem);

        let initial_len = out.len();
        // Use index string only if needed (circular reference error). Avoids i.to_string() alloc.
        let elem_key = i.to_string();
        let written = serialize_value_simple(
            &elem,
            &elem_key,
            indent,
            property_list,
            tracker,
            depth + 1,
            ncx,
            out,
        )?;
        if !written {
            out.truncate(initial_len);
            out.push_str("null");
        }
    }

    if let Some(ind) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(ind);
        }
    }
    out.push(']');

    tracker.exit(ptr);
    Ok(())
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
    out: &mut String,
) -> Result<(), VmError> {
    let obj = value
        .as_object()
        .ok_or_else(|| VmError::type_error("Expected object"))?;

    // Check for circular reference
    let ptr = obj.as_ptr() as usize;
    if let Err(msg) = tracker.enter(obj_key, ptr, false) {
        return Err(VmError::type_error(msg));
    }

    // Fast path for shape-based objects (non-dictionary): iterate shape keys + offsets
    // in one pass, avoiding separate own_keys + get_own_property_descriptor + get lookups.
    if property_list.is_none() && !obj.is_dictionary_mode() {
        let shape_keys = obj.own_keys(); // shape keys in insertion order
        if shape_keys.is_empty() {
            out.push_str("{}");
            tracker.exit(ptr);
            return Ok(());
        }

        out.push('{');
        let mut first = true;

        for (i, prop_key) in shape_keys.iter().enumerate() {
            maybe_check_interrupt(ncx, i)?;
            // Skip symbols
            if matches!(prop_key, PropertyKey::Symbol(_)) {
                continue;
            }
            // Get offset from shape and read value + descriptor in one lookup
            let offset = obj.shape_get_offset(prop_key);
            let val = if let Some(off) = offset {
                match obj.get_property_entry_by_offset(off) {
                    Some(desc) => {
                        if !desc.enumerable() {
                            continue;
                        }
                        desc.value().cloned().unwrap_or(Value::undefined())
                    }
                    None => continue,
                }
            } else {
                continue;
            };
            let val = unwrap_primitive(&val);

            // Get key string
            let key_str: std::borrow::Cow<str> = match prop_key {
                PropertyKey::String(s) => std::borrow::Cow::Borrowed(s.as_str()),
                PropertyKey::Index(idx) => std::borrow::Cow::Owned(idx.to_string()),
                PropertyKey::Symbol(_) => unreachable!(),
            };

            let initial_len = out.len();
            if !first {
                out.push(',');
            }
            if let Some(ind) = indent {
                out.push('\n');
                for _ in 0..=depth {
                    out.push_str(ind);
                }
            }

            out.push('"');
            escape_json_string(&key_str, out);
            out.push('"');
            out.push(':');
            if indent.is_some() {
                out.push(' ');
            }

            let written = serialize_value_simple(
                &val,
                &key_str,
                indent,
                property_list,
                tracker,
                depth + 1,
                ncx,
                out,
            )?;
            if written {
                first = false;
            } else {
                out.truncate(initial_len);
            }
        }

        if let Some(ind) = indent {
            out.push('\n');
            for _ in 0..depth {
                out.push_str(ind);
            }
        }
        out.push('}');
        tracker.exit(ptr);
        return Ok(());
    }

    // Slow path: dictionary mode or with property_list replacer
    let keys: Vec<String> = if let Some(list) = property_list {
        list.clone()
    } else {
        obj.own_keys()
            .into_iter()
            .filter_map(|k| {
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
                            PropertyKey::Symbol(_) => None,
                        };
                    }
                }
                None
            })
            .collect()
    };

    if keys.is_empty() {
        out.push_str("{}");
        tracker.exit(ptr);
        return Ok(());
    }

    out.push('{');
    let mut first = true;
    let mut wrote_property = false;

    for (i, key) in keys.into_iter().enumerate() {
        maybe_check_interrupt(ncx, i)?;
        if let Some(val) = obj.get(&PropertyKey::string(&key)) {
            let val = unwrap_primitive(&val);

            let initial_len = out.len();
            if !first {
                out.push(',');
            }
            if let Some(ind) = indent {
                out.push('\n');
                for _ in 0..=depth {
                    out.push_str(ind);
                }
            }

            out.push('"');
            escape_json_string(&key, out);
            out.push('"');
            out.push(':');
            if indent.is_some() {
                out.push(' ');
            }

            let written = serialize_value_simple(
                &val,
                &key,
                indent,
                property_list,
                tracker,
                depth + 1,
                ncx,
                out,
            )?;
            if written {
                first = false;
                wrote_property = true;
            } else {
                out.truncate(initial_len);
            }
        }
    }

    if wrote_property && indent.is_some() {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent.as_ref().unwrap());
        }
    } else if !wrote_property && out.len() > 1 {
        // Did not write properties, so we shouldn't have newlines
        // If we added '{', do nothing more
    }
    out.push('}');

    tracker.exit(ptr);
    Ok(())
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
    out: &mut String,
) -> Result<bool, VmError> {
    let prop_key = PropertyKey::string(key);
    stringify_with_replacer_prepared(
        holder,
        key,
        prop_key,
        replacer_fn,
        indent,
        property_list,
        tracker,
        depth,
        ncx,
        out,
    )
}

fn stringify_with_replacer_prepared(
    holder: &Value,
    key: &str,
    prop_key: PropertyKey,
    replacer_fn: &Option<Value>,
    indent: &Option<String>,
    property_list: &Option<Vec<String>>,
    tracker: &mut CircularTracker,
    depth: usize,
    ncx: &mut NativeContext,
    out: &mut String,
) -> Result<bool, VmError> {
    // Depth limit
    if depth > 100 {
        out.push_str("null");
        return Ok(true);
    }

    // Step 2: Get value from holder (properly invoking getters)
    let value = if let Some(obj) = holder.as_object().or_else(|| holder.as_array()) {
        get_property_value(&obj, &prop_key, holder, ncx)?
    } else if let Some(proxy) = holder.as_proxy() {
        // For proxies, invoke the get trap
        let access_key_value = stringify_access_key_value_for_proxy(prop_key, key);
        crate::proxy_operations::proxy_get(ncx, proxy, &prop_key, access_key_value, holder.clone())?
    } else {
        return Ok(false);
    };

    // Step 3: Call toJSON if present
    let value = call_to_json(&value, key, prop_key, ncx)?;

    // Step 4: Call replacer function if present
    let value = call_replacer(replacer_fn, holder, key, prop_key, value, ncx)?;

    // Step 5: Unwrap wrapper objects (calls ToString for String wrappers per spec)
    let value = unwrap_primitive_with_calls(&value, ncx)?;

    // Step 6: Serialize based on type
    // undefined, functions, symbols return false (omitted)
    if value.is_undefined() || value.is_callable() || value.is_symbol() {
        return Ok(false);
    }

    // null
    if value.is_null() {
        out.push_str("null");
        return Ok(true);
    }

    // BigInt should have been handled by toJSON or should throw
    if value.is_bigint() {
        return Err(VmError::type_error("Do not know how to serialize a BigInt"));
    }

    // Boolean
    if let Some(b) = value.as_boolean() {
        out.push_str(if b { "true" } else { "false" });
        return Ok(true);
    }

    // Number (int32 or f64)
    if let Some(n) = value.as_int32() {
        let _ = write!(out, "{}", n);
        return Ok(true);
    }
    if let Some(n) = value.as_number() {
        format_number(n, out);
        return Ok(true);
    }

    // String - use UTF-16 escaping to preserve lone surrogates
    if let Some(s) = value.as_string() {
        out.push('"');
        escape_json_string_utf16(s.as_utf16(), out);
        out.push('"');
        return Ok(true);
    }

    // Check for array (including proxy arrays)
    if is_array_value(&value)? {
        stringify_array_with_replacer(
            &value,
            key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth,
            ncx,
            out,
        )?;
        return Ok(true);
    }

    // Regular object or proxy
    if value.as_object().is_some() || value.as_proxy().is_some() {
        stringify_object_with_replacer(
            &value,
            key,
            replacer_fn,
            indent,
            property_list,
            tracker,
            depth,
            ncx,
            out,
        )?;
        return Ok(true);
    }

    out.push_str("null");
    Ok(true)
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
    out: &mut String,
) -> Result<(), VmError> {
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

    if len == 0 {
        out.push_str("[]");
        tracker.exit(ptr);
        return Ok(());
    }

    out.push('[');
    if let Some(ind) = indent {
        out.push('\n');
    }

    let mut index_key_text = String::new();
    for i in 0..len {
        maybe_check_interrupt(ncx, i)?;
        index_key_text.clear();
        let _ = std::fmt::Write::write_fmt(&mut index_key_text, format_args!("{i}"));

        if i > 0 {
            out.push(',');
            if let Some(ind) = indent {
                out.push('\n');
            }
        }
        if let Some(ind) = indent {
            for _ in 0..=depth {
                out.push_str(ind);
            }
        }

        let initial_len = out.len();
        let written = if let Ok(index) = u32::try_from(i) {
            stringify_with_replacer_prepared(
                value,
                &index_key_text,
                PropertyKey::Index(index),
                replacer_fn,
                indent,
                property_list,
                tracker,
                depth + 1,
                ncx,
                out,
            )?
        } else {
            stringify_with_replacer(
                value,
                &index_key_text,
                replacer_fn,
                indent,
                property_list,
                tracker,
                depth + 1,
                ncx,
                out,
            )?
        };

        if !written {
            out.truncate(initial_len);
            out.push_str("null");
        }
    }

    if let Some(ind) = indent {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(ind);
        }
    }
    out.push(']');

    tracker.exit(ptr);
    Ok(())
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
    out: &mut String,
) -> Result<(), VmError> {
    enum ObjectStringifyKey {
        Prepared(PropertyKey),
        Text(String),
    }

    // Get pointer for circular reference checking - works for objects and proxies
    let (ptr, keys) = if let Some(obj) = value.as_object() {
        let ptr = obj.as_ptr() as usize;
        let keys: Vec<ObjectStringifyKey> = if let Some(list) = property_list {
            list.iter().cloned().map(ObjectStringifyKey::Text).collect()
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
                            return match k {
                                PropertyKey::String(_) | PropertyKey::Index(_) => {
                                    Some(ObjectStringifyKey::Prepared(k))
                                }
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
        let keys: Vec<ObjectStringifyKey> = if let Some(list) = property_list {
            list.iter().cloned().map(ObjectStringifyKey::Text).collect()
        } else {
            // Get keys from proxy using ownKeys trap
            let proxy_keys = crate::proxy_operations::proxy_own_keys(ncx, proxy)?;
            proxy_keys
                .into_iter()
                .filter_map(|k| match k {
                    PropertyKey::String(s) => {
                        Some(ObjectStringifyKey::Text(s.as_str().to_string()))
                    }
                    PropertyKey::Index(i) => Some(ObjectStringifyKey::Text(i.to_string())),
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

    if keys.is_empty() {
        out.push_str("{}");
        tracker.exit(ptr);
        return Ok(());
    }

    out.push('{');
    let mut first = true;
    let mut wrote_property = false;

    for (i, key_entry) in keys.into_iter().enumerate() {
        maybe_check_interrupt(ncx, i)?;
        let initial_len = out.len();

        match key_entry {
            ObjectStringifyKey::Prepared(prop_key) => match prop_key {
                PropertyKey::String(s) => {
                    let key_text = s.as_str();
                    if !first {
                        out.push(',');
                    }
                    if let Some(ind) = indent {
                        out.push('\n');
                        for _ in 0..=depth {
                            out.push_str(ind);
                        }
                    }

                    out.push('"');
                    escape_json_string(key_text, out);
                    out.push('"');
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }

                    let written = stringify_with_replacer_prepared(
                        value,
                        key_text,
                        prop_key,
                        replacer_fn,
                        indent,
                        property_list,
                        tracker,
                        depth + 1,
                        ncx,
                        out,
                    )?;

                    if written {
                        first = false;
                        wrote_property = true;
                    } else {
                        out.truncate(initial_len);
                    }
                }
                PropertyKey::Index(i) => {
                    let key_text = i.to_string();
                    if !first {
                        out.push(',');
                    }
                    if let Some(ind) = indent {
                        out.push('\n');
                        for _ in 0..=depth {
                            out.push_str(ind);
                        }
                    }

                    out.push('"');
                    escape_json_string(&key_text, out);
                    out.push('"');
                    out.push(':');
                    if indent.is_some() {
                        out.push(' ');
                    }

                    let written = stringify_with_replacer_prepared(
                        value,
                        &key_text,
                        prop_key,
                        replacer_fn,
                        indent,
                        property_list,
                        tracker,
                        depth + 1,
                        ncx,
                        out,
                    )?;

                    if written {
                        first = false;
                        wrote_property = true;
                    } else {
                        out.truncate(initial_len);
                    }
                }
                PropertyKey::Symbol(_) => continue,
            },
            ObjectStringifyKey::Text(key) => {
                if !first {
                    out.push(',');
                }
                if let Some(ind) = indent {
                    out.push('\n');
                    for _ in 0..=depth {
                        out.push_str(ind);
                    }
                }

                out.push('"');
                escape_json_string(&key, out);
                out.push('"');
                out.push(':');
                if indent.is_some() {
                    out.push(' ');
                }

                let written = stringify_with_replacer(
                    value,
                    &key,
                    replacer_fn,
                    indent,
                    property_list,
                    tracker,
                    depth + 1,
                    ncx,
                    out,
                )?;

                if written {
                    first = false;
                    wrote_property = true;
                } else {
                    out.truncate(initial_len);
                }
            }
        }
    }

    if wrote_property && indent.is_some() {
        out.push('\n');
        for _ in 0..depth {
            out.push_str(indent.as_ref().unwrap());
        }
    }
    out.push('}');

    tracker.exit(ptr);
    Ok(())
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
    let result = parse_json_to_value_direct(&text, &mm, &object_proto, &array_proto, ncx)?;

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

    let mut tracker = CircularTracker::new();
    let mut out = String::with_capacity(128);

    // Fast path: no replacer, no toJSON on top-level → skip wrapper object allocation
    let written = if replacer_fn.is_none() {
        serialize_value_simple(
            &val,
            "",
            &space_str,
            &property_list,
            &mut tracker,
            0,
            ncx,
            &mut out,
        )?
    } else {
        // Slow path: create wrapper object for replacer spec compliance
        let global = ncx.ctx.global();
        let object_proto = global
            .get(&PropertyKey::string("Object"))
            .and_then(|o| o.as_object())
            .and_then(|o| o.get(&PropertyKey::string("prototype")))
            .unwrap_or_else(Value::null);
        let wrapper = GcRef::new(JsObject::new(object_proto));
        let _ = wrapper.set(PropertyKey::string(""), val.clone());
        let wrapper_val = Value::object(wrapper);

        stringify_with_replacer(
            &wrapper_val,
            "",
            &replacer_fn,
            &space_str,
            &property_list,
            &mut tracker,
            0,
            ncx,
            &mut out,
        )?
    };

    if written {
        Ok(Value::string(JsString::intern(&out)))
    } else {
        Ok(Value::undefined())
    }
}

/// Create and install JSON namespace on global object
pub fn install_json_namespace(
    global: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    function_prototype: GcRef<JsObject>,
) {
    let json_obj = GcRef::new(JsObject::new(Value::null()));

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
        let mut seen = FxHashSet::default();

        for i in 0..len {
            maybe_check_interrupt(ncx, i)?;
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
    let root = GcRef::new(JsObject::new(Value::null()));
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
            maybe_check_interrupt(ncx, i)?;
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
        for (i, key_str) in keys.into_iter().enumerate() {
            maybe_check_interrupt(ncx, i)?;
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
