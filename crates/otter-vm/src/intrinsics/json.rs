//! JSON namespace — ES2024 §25.5.
//!
//! Performance-oriented implementation:
//! - `JSON.parse` uses `serde_json::Deserializer` with a custom `Visitor` that
//!   builds VM heap values directly from the token stream.  No intermediate
//!   `serde_json::Value` tree — single allocation pass.  Integer fast path
//!   for i32 values.  String keys interned on the fly.
//! - `JSON.stringify` is hand-written: `itoa` for integer formatting, pre-computed
//!   escape table, cycle detection via handle vec, depth-limited recursion.
//! - Both paths cap depth at 512 to prevent stack overflow.

use serde::de::{self, DeserializeSeed, MapAccess, SeqAccess, Visitor};

use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{HeapValueKind, ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller},
};

pub(super) static JSON_INTRINSIC: JsonIntrinsic = JsonIntrinsic;

pub(super) struct JsonIntrinsic;

const MAX_DEPTH: usize = 512;

impl IntrinsicInstaller for JsonIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let namespace = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
        intrinsics.set_namespace("JSON", namespace);

        let parse_desc = NativeFunctionDescriptor::method("parse", 2, json_parse);
        let parse_id = cx.native_functions.register(parse_desc);
        let parse_fn =
            cx.alloc_intrinsic_host_function(parse_id, intrinsics.function_prototype())?;
        let parse_prop = cx.property_names.intern("parse");
        cx.heap.set_property(
            namespace,
            parse_prop,
            RegisterValue::from_object_handle(parse_fn.0),
        )?;

        let stringify_desc = NativeFunctionDescriptor::method("stringify", 3, json_stringify);
        let stringify_id = cx.native_functions.register(stringify_desc);
        let stringify_fn =
            cx.alloc_intrinsic_host_function(stringify_id, intrinsics.function_prototype())?;
        let stringify_prop = cx.property_names.intern("stringify");
        cx.heap.set_property(
            namespace,
            stringify_prop,
            RegisterValue::from_object_handle(stringify_fn.0),
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        if let Some(namespace) = intrinsics.namespace("JSON") {
            cx.install_global_value(
                intrinsics,
                "JSON",
                RegisterValue::from_object_handle(namespace.0),
            )?;
        }
        Ok(())
    }
}

// ═══════════════════════════════════════════════════════════════════════════
//  JSON.parse — ES2024 §25.5.1 — streaming visitor, zero intermediate tree
// ═══════════════════════════════════════════════════════════════════════════

/// ES2024 §25.5.1 JSON.parse(text [, reviver])
fn json_parse(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let text = args
        .first()
        .copied()
        .map(|v| runtime.js_to_string(v).map_err(|e| map_error(e, runtime)))
        .transpose()?
        .unwrap_or_else(|| "undefined".into());

    let reviver = args
        .get(1)
        .copied()
        .and_then(|v| v.as_object_handle().map(ObjectHandle))
        .filter(|h| runtime.objects().is_callable(*h));

    // Stream-parse JSON directly into VM heap values.
    let mut deserializer = serde_json::Deserializer::from_str(&text);
    let seed = HeapSeed { runtime, depth: 0 };
    let result = seed
        .deserialize(&mut deserializer)
        .map_err(|e| syntax_error(runtime, &format!("JSON.parse: {e}")))?;

    // Ensure no trailing content.
    deserializer
        .end()
        .map_err(|e| syntax_error(runtime, &format!("JSON.parse: {e}")))?;

    // Apply reviver if provided.
    if let Some(reviver_fn) = reviver {
        let root = runtime.alloc_object();
        let empty_key = runtime.intern_property_name("");
        runtime
            .objects_mut()
            .set_property(root, empty_key, result)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        let key_str = runtime.alloc_string("");
        walk_reviver(
            root,
            RegisterValue::from_object_handle(key_str.0),
            reviver_fn,
            runtime,
            0,
        )
    } else {
        Ok(result)
    }
}

/// Seed that carries a mutable runtime reference + depth counter.
/// Implements `DeserializeSeed` so serde calls our `Visitor` to build heap values.
struct HeapSeed<'a> {
    runtime: &'a mut crate::interpreter::RuntimeState,
    depth: usize,
}

impl<'de, 'a> DeserializeSeed<'de> for HeapSeed<'a> {
    type Value = RegisterValue;

    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        deserializer: D,
    ) -> Result<RegisterValue, D::Error> {
        if self.depth > MAX_DEPTH {
            return Err(de::Error::custom("JSON.parse: too deeply nested"));
        }
        deserializer.deserialize_any(HeapVisitor {
            runtime: self.runtime,
            depth: self.depth,
        })
    }
}

/// Visitor that builds VM heap values directly from the serde token stream.
struct HeapVisitor<'a> {
    runtime: &'a mut crate::interpreter::RuntimeState,
    depth: usize,
}

impl<'de, 'a> Visitor<'de> for HeapVisitor<'a> {
    type Value = RegisterValue;

    fn expecting(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("any JSON value")
    }

    fn visit_bool<E: de::Error>(self, v: bool) -> Result<RegisterValue, E> {
        Ok(RegisterValue::from_bool(v))
    }

    fn visit_i64<E: de::Error>(self, v: i64) -> Result<RegisterValue, E> {
        if let Ok(i) = i32::try_from(v) {
            Ok(RegisterValue::from_i32(i))
        } else {
            Ok(RegisterValue::from_number(v as f64))
        }
    }

    fn visit_u64<E: de::Error>(self, v: u64) -> Result<RegisterValue, E> {
        if let Ok(i) = i32::try_from(v) {
            Ok(RegisterValue::from_i32(i))
        } else {
            Ok(RegisterValue::from_number(v as f64))
        }
    }

    fn visit_f64<E: de::Error>(self, v: f64) -> Result<RegisterValue, E> {
        Ok(RegisterValue::from_number(v))
    }

    fn visit_str<E: de::Error>(self, v: &str) -> Result<RegisterValue, E> {
        let handle = self.runtime.alloc_string(v);
        Ok(RegisterValue::from_object_handle(handle.0))
    }

    fn visit_string<E: de::Error>(self, v: String) -> Result<RegisterValue, E> {
        let handle = self.runtime.alloc_string(v);
        Ok(RegisterValue::from_object_handle(handle.0))
    }

    fn visit_unit<E: de::Error>(self) -> Result<RegisterValue, E> {
        Ok(RegisterValue::null())
    }

    fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<RegisterValue, A::Error> {
        let handle = self.runtime.alloc_array();
        // Pre-allocate if size hint is available.
        if let Some(hint) = seq.size_hint() {
            self.runtime
                .objects_mut()
                .set_array_length(handle, hint)
                .ok();
        }
        let mut index = 0usize;
        while let Some(elem) = seq.next_element_seed(HeapSeed {
            runtime: self.runtime,
            depth: self.depth + 1,
        })? {
            self.runtime
                .objects_mut()
                .set_index(handle, index, elem)
                .ok();
            index += 1;
        }
        Ok(RegisterValue::from_object_handle(handle.0))
    }

    fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<RegisterValue, A::Error> {
        let handle = self.runtime.alloc_object();
        while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
            let prop = self.runtime.intern_property_name(&key);
            let value = map.next_value_seed(HeapSeed {
                runtime: self.runtime,
                depth: self.depth + 1,
            })?;
            self.runtime
                .objects_mut()
                .set_property(handle, prop, value)
                .map_err(|e| de::Error::custom(format!("{e:?}")))?;
        }
        Ok(RegisterValue::from_object_handle(handle.0))
    }
}

/// ES2024 §25.5.1.1 InternalizeJSONProperty — walk the reviver.
fn walk_reviver(
    holder: ObjectHandle,
    key: RegisterValue,
    reviver: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
    depth: usize,
) -> Result<RegisterValue, VmNativeCallError> {
    if depth > MAX_DEPTH {
        return Ok(RegisterValue::undefined());
    }

    let key_str = runtime
        .js_to_string(key)
        .map_err(|e| map_error(e, runtime))?;
    let property = runtime.intern_property_name(&key_str);
    let val = match runtime
        .objects()
        .get_property(holder, property)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
    {
        Some(lookup) => match lookup.value() {
            PropertyValue::Data { value, .. } => value,
            _ => RegisterValue::undefined(),
        },
        None => RegisterValue::undefined(),
    };

    if let Some(obj_handle) = val.as_object_handle().map(ObjectHandle) {
        match runtime.objects().kind(obj_handle) {
            Ok(HeapValueKind::Array) => {
                let length = runtime
                    .objects()
                    .array_length(obj_handle)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
                    .unwrap_or(0);
                for i in 0..length {
                    let idx_str = runtime.alloc_string(i.to_string());
                    let new_val = walk_reviver(
                        obj_handle,
                        RegisterValue::from_object_handle(idx_str.0),
                        reviver,
                        runtime,
                        depth + 1,
                    )?;
                    if new_val == RegisterValue::undefined() {
                        let idx_prop = runtime.intern_property_name(&i.to_string());
                        let names = runtime.property_names().clone();
                        runtime
                            .objects_mut()
                            .delete_property_with_registry(obj_handle, idx_prop, &names)
                            .ok();
                    } else {
                        runtime.objects_mut().set_index(obj_handle, i, new_val).ok();
                    }
                }
            }
            Ok(HeapValueKind::Object) => {
                let property_names = runtime.property_names().clone();
                let mut pn_clone = property_names.clone();
                let keys = runtime
                    .objects()
                    .own_keys_with_registry(obj_handle, &mut pn_clone)
                    .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
                for prop_id in keys {
                    let prop_name = property_names.get(prop_id).unwrap_or("").to_string();
                    let prop_str = runtime.alloc_string(&*prop_name);
                    let new_val = walk_reviver(
                        obj_handle,
                        RegisterValue::from_object_handle(prop_str.0),
                        reviver,
                        runtime,
                        depth + 1,
                    )?;
                    if new_val == RegisterValue::undefined() {
                        let names = runtime.property_names().clone();
                        runtime
                            .objects_mut()
                            .delete_property_with_registry(obj_handle, prop_id, &names)
                            .ok();
                    } else {
                        runtime
                            .objects_mut()
                            .set_property(obj_handle, prop_id, new_val)
                            .ok();
                    }
                }
            }
            _ => {}
        }
    }

    // Call reviver(holder, key, val).
    let holder_val = RegisterValue::from_object_handle(holder.0);
    let current_val = match runtime
        .objects()
        .get_property(holder, property)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
    {
        Some(lookup) => match lookup.value() {
            PropertyValue::Data { value, .. } => value,
            _ => RegisterValue::undefined(),
        },
        None => RegisterValue::undefined(),
    };
    runtime.call_callable(reviver, holder_val, &[key, current_val])
}

// ═══════════════════════════════════════════════════════════════════════════
//  JSON.stringify — ES2024 §25.5.2
// ═══════════════════════════════════════════════════════════════════════════

// Pre-computed escape table for bytes 0..128.
static ESCAPE_TABLE: [&str; 128] = {
    let mut table = [""; 128];
    table[b'"' as usize] = "\\\"";
    table[b'\\' as usize] = "\\\\";
    table[b'\n' as usize] = "\\n";
    table[b'\r' as usize] = "\\r";
    table[b'\t' as usize] = "\\t";
    table[0x08] = "\\b";
    table[0x0C] = "\\f";
    table
};

/// ES2024 §25.5.2 JSON.stringify(value [, replacer [, space]])
fn json_stringify(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let replacer_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let space_arg = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let replacer_fn = replacer_arg
        .as_object_handle()
        .map(ObjectHandle)
        .filter(|h| runtime.objects().is_callable(*h));
    let replacer_list: Option<Vec<String>> = if replacer_fn.is_none() {
        replacer_arg
            .as_object_handle()
            .map(ObjectHandle)
            .filter(|h| matches!(runtime.objects().kind(*h), Ok(HeapValueKind::Array)))
            .and_then(|h| {
                let len = runtime.objects().array_length(h).ok()?.unwrap_or(0);
                let mut list = Vec::with_capacity(len);
                for i in 0..len {
                    if let Ok(Some(v)) = runtime.get_array_index_value(h, i)
                        && let Ok(s) = runtime.js_to_string(v)
                    {
                        list.push(s.into_string());
                    }
                }
                Some(list)
            })
    } else {
        None
    };

    let indent = resolve_indent(space_arg, runtime);

    let mut out = String::with_capacity(128);
    let mut visited = Vec::new();

    let success = stringify_value(
        value,
        &indent,
        "",
        replacer_fn,
        replacer_list.as_deref(),
        &mut visited,
        &mut out,
        runtime,
        0,
    )?;

    if success {
        let handle = runtime.alloc_string(out);
        Ok(RegisterValue::from_object_handle(handle.0))
    } else {
        Ok(RegisterValue::undefined())
    }
}

fn resolve_indent(space: RegisterValue, runtime: &mut crate::interpreter::RuntimeState) -> String {
    if space == RegisterValue::undefined() || space == RegisterValue::null() {
        return String::new();
    }
    if let Some(n) = space
        .as_i32()
        .or_else(|| space.as_number().map(|f| f as i32))
    {
        let n = n.clamp(0, 10) as usize;
        " ".repeat(n)
    } else if let Some(handle) = space.as_object_handle().map(ObjectHandle) {
        if let Ok(Some(s)) = runtime.objects().string_value(handle) {
            let s = s.to_string();
            if s.len() > 10 { s[..10].to_string() } else { s }
        } else {
            String::new()
        }
    } else {
        String::new()
    }
}

#[allow(clippy::too_many_arguments)]
fn stringify_value(
    value: RegisterValue,
    indent: &str,
    current_indent: &str,
    replacer_fn: Option<ObjectHandle>,
    replacer_list: Option<&[String]>,
    visited: &mut Vec<u32>,
    out: &mut String,
    runtime: &mut crate::interpreter::RuntimeState,
    depth: usize,
) -> Result<bool, VmNativeCallError> {
    if depth > MAX_DEPTH {
        out.push_str("null");
        return Ok(true);
    }

    // toJSON check.
    let value = apply_to_json(value, runtime)?;

    // Primitives — fast path, no heap access needed.
    if value == RegisterValue::null() {
        out.push_str("null");
        return Ok(true);
    }
    if value == RegisterValue::undefined() {
        return Ok(false);
    }
    if let Some(b) = value.as_bool() {
        out.push_str(if b { "true" } else { "false" });
        return Ok(true);
    }
    if value.is_symbol() {
        return Ok(false);
    }
    // Integer fast path — itoa, no allocation.
    if let Some(n) = value.as_i32() {
        let mut buf = itoa::Buffer::new();
        out.push_str(buf.format(n));
        return Ok(true);
    }
    if let Some(n) = value.as_number() {
        if n.is_nan() || n.is_infinite() {
            out.push_str("null");
        } else if n.fract() == 0.0 && n.abs() < 1e20 {
            let mut buf = itoa::Buffer::new();
            out.push_str(buf.format(n as i64));
        } else {
            let mut buf = ryu::Buffer::new();
            out.push_str(buf.format(n));
        }
        return Ok(true);
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Ok(false);
    };

    // String values.
    if let Ok(Some(s)) = runtime.objects().string_value(handle) {
        escape_json_string(s, out);
        return Ok(true);
    }

    // Functions are omitted.
    if runtime.objects().is_callable(handle) {
        return Ok(false);
    }

    // Cycle detection.
    if visited.contains(&handle.0) {
        return Err(type_error(runtime, "Converting circular structure to JSON"));
    }
    visited.push(handle.0);

    let result = match runtime.objects().kind(handle) {
        Ok(HeapValueKind::Array) => stringify_array(
            handle,
            indent,
            current_indent,
            replacer_fn,
            replacer_list,
            visited,
            out,
            runtime,
            depth,
        ),
        _ => stringify_object(
            handle,
            indent,
            current_indent,
            replacer_fn,
            replacer_list,
            visited,
            out,
            runtime,
            depth,
        ),
    };

    visited.pop();
    result
}

#[allow(clippy::too_many_arguments)]
fn stringify_array(
    handle: ObjectHandle,
    indent: &str,
    current_indent: &str,
    replacer_fn: Option<ObjectHandle>,
    replacer_list: Option<&[String]>,
    visited: &mut Vec<u32>,
    out: &mut String,
    runtime: &mut crate::interpreter::RuntimeState,
    depth: usize,
) -> Result<bool, VmNativeCallError> {
    let length = runtime
        .objects()
        .array_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
        .unwrap_or(0);

    out.push('[');

    let child_indent = if indent.is_empty() {
        String::new()
    } else {
        format!("{current_indent}{indent}")
    };

    let mut first = true;
    for i in 0..length {
        if !first {
            out.push(',');
        }
        if !indent.is_empty() {
            out.push('\n');
            out.push_str(&child_indent);
        }

        let elem = runtime
            .get_array_index_value(handle, i)
            .unwrap_or(Some(RegisterValue::undefined()))
            .unwrap_or_else(RegisterValue::undefined);

        let elem = if let Some(replacer) = replacer_fn {
            let idx_str = runtime.alloc_string(i.to_string());
            let holder = RegisterValue::from_object_handle(handle.0);
            runtime.call_callable(
                replacer,
                holder,
                &[RegisterValue::from_object_handle(idx_str.0), elem],
            )?
        } else {
            elem
        };

        let wrote = stringify_value(
            elem,
            indent,
            &child_indent,
            replacer_fn,
            replacer_list,
            visited,
            out,
            runtime,
            depth + 1,
        )?;
        if !wrote {
            out.push_str("null");
        }
        first = false;
    }

    if !indent.is_empty() && length > 0 {
        out.push('\n');
        out.push_str(current_indent);
    }
    out.push(']');
    Ok(true)
}

#[allow(clippy::too_many_arguments)]
fn stringify_object(
    handle: ObjectHandle,
    indent: &str,
    current_indent: &str,
    replacer_fn: Option<ObjectHandle>,
    replacer_list: Option<&[String]>,
    visited: &mut Vec<u32>,
    out: &mut String,
    runtime: &mut crate::interpreter::RuntimeState,
    depth: usize,
) -> Result<bool, VmNativeCallError> {
    out.push('{');

    let child_indent = if indent.is_empty() {
        String::new()
    } else {
        format!("{current_indent}{indent}")
    };

    // Collect own enumerable key-value pairs.
    // Use own_keys (no registry needed for plain objects) then resolve names.
    let keys = runtime
        .objects()
        .own_keys(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    // Pre-resolve names and values to avoid repeated borrow conflicts.
    let mut entries: Vec<(String, RegisterValue)> = Vec::with_capacity(keys.len());
    // First pass: resolve names to owned strings (avoids borrow conflicts).
    let key_names: Vec<(usize, String)> = keys
        .iter()
        .enumerate()
        .filter_map(|(i, prop_id)| {
            runtime
                .property_names()
                .get(*prop_id)
                .map(|n| (i, n.to_string()))
        })
        .collect();

    let pn = runtime.property_names().clone();
    for (idx, prop_name) in &key_names {
        let prop_id = keys[*idx];

        // Replacer list filter.
        if let Some(list) = replacer_list
            && !list.iter().any(|s| s == prop_name)
        {
            continue;
        }

        let desc = runtime
            .objects()
            .own_property_descriptor(handle, prop_id, &pn)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        let Some(pv) = desc else { continue };
        if !pv.attributes().enumerable() {
            continue;
        }

        let val = match pv {
            PropertyValue::Data { value, .. } => value,
            PropertyValue::Accessor { getter, .. } => {
                if let Some(getter) = getter {
                    runtime
                        .call_callable(getter, RegisterValue::from_object_handle(handle.0), &[])
                        .unwrap_or_else(|_| RegisterValue::undefined())
                } else {
                    RegisterValue::undefined()
                }
            }
        };

        entries.push((prop_name.clone(), val));
    }

    let mut first = true;
    for (prop_name, val) in entries {
        let val = if let Some(replacer) = replacer_fn {
            let key_str = runtime.alloc_string(&*prop_name);
            let holder = RegisterValue::from_object_handle(handle.0);
            runtime.call_callable(
                replacer,
                holder,
                &[RegisterValue::from_object_handle(key_str.0), val],
            )?
        } else {
            val
        };

        let mark = out.len();
        if !first {
            out.push(',');
        }
        if !indent.is_empty() {
            out.push('\n');
            out.push_str(&child_indent);
        }
        escape_json_string(&prop_name, out);
        out.push(':');
        if !indent.is_empty() {
            out.push(' ');
        }

        let wrote = stringify_value(
            val,
            indent,
            &child_indent,
            replacer_fn,
            replacer_list,
            visited,
            out,
            runtime,
            depth + 1,
        )?;
        if !wrote {
            out.truncate(mark);
        } else {
            first = false;
        }
    }

    if !indent.is_empty() && !first {
        out.push('\n');
        out.push_str(current_indent);
    }
    out.push('}');
    Ok(true)
}

fn apply_to_json(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Ok(value);
    };
    if runtime
        .objects()
        .string_value(handle)
        .ok()
        .flatten()
        .is_some()
    {
        return Ok(value);
    }
    let to_json_prop = runtime.intern_property_name("toJSON");
    let to_json = runtime
        .ordinary_get(handle, to_json_prop, value)
        .unwrap_or_else(|_| RegisterValue::undefined());
    if let Some(callable) = to_json.as_object_handle().map(ObjectHandle)
        && runtime.objects().is_callable(callable)
    {
        return runtime.call_callable(callable, value, &[]);
    }
    Ok(value)
}

/// Escape a string for JSON output. Handles ASCII control chars, quote, backslash.
fn escape_json_string(s: &str, out: &mut String) {
    out.push('"');
    let bytes = s.as_bytes();
    let mut i = 0;
    let mut last_flush = 0;
    while i < bytes.len() {
        let b = bytes[i];
        if b < 0x20 || b == b'"' || b == b'\\' {
            // Flush safe run.
            if last_flush < i {
                out.push_str(&s[last_flush..i]);
            }
            if (b as usize) < ESCAPE_TABLE.len() {
                let esc = ESCAPE_TABLE[b as usize];
                if !esc.is_empty() {
                    out.push_str(esc);
                } else {
                    let _ = std::fmt::Write::write_fmt(out, format_args!("\\u{b:04x}"));
                }
            }
            last_flush = i + 1;
        }
        i += 1;
    }
    // Flush remaining safe bytes.
    if last_flush < bytes.len() {
        out.push_str(&s[last_flush..]);
    }
    out.push('"');
}

// ═══════════════════════════════════════════════════════════════════════════
//  Helpers
// ═══════════════════════════════════════════════════════════════════════════

fn syntax_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    let prototype = runtime.intrinsics().syntax_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg = runtime.alloc_string(message);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
        .ok();
    let name = runtime.alloc_string("SyntaxError");
    let name_prop = runtime.intern_property_name("name");
    runtime
        .objects_mut()
        .set_property(handle, name_prop, RegisterValue::from_object_handle(name.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

fn type_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(handle) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0)),
        Err(error) => VmNativeCallError::Internal(format!("{error}").into()),
    }
}

fn map_error(
    error: crate::interpreter::InterpreterError,
    runtime: &mut crate::interpreter::RuntimeState,
) -> VmNativeCallError {
    match error {
        crate::interpreter::InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
        crate::interpreter::InterpreterError::TypeError(msg) => type_error(runtime, &msg),
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}
