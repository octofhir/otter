//! Array constructor statics and prototype methods (ES2026)
//!
//! ## Constructor statics:
//! - `Array.isArray()`, `Array.from()`, `Array.of()`
//!
//! ## Prototype methods:
//! - push, pop, shift, unshift, indexOf, lastIndexOf, includes, join, toString,
//!   slice, concat, reverse, at, fill, splice, flat, forEach, map, filter,
//!   reduce, reduceRight, find, findIndex, every, some, sort, entries, keys, values, copyWithin

use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
use crate::intrinsics_impl::helpers::{same_value_zero, strict_equal};
use std::sync::Arc;

/// Helper: get array length from an object (spec-compliant, no cap).
fn get_len(obj: &GcRef<JsObject>) -> usize {
    obj.get(&PropertyKey::string("length"))
        .and_then(|v| v.as_number())
        .unwrap_or(0.0) as usize
}

/// Helper: convert value to string for default sort comparison
fn value_to_sort_string(val: &Value) -> String {
    if val.is_undefined() {
        return String::new(); // undefined sorts last but we handle that
    }
    if let Some(s) = val.as_string() {
        return s.as_str().to_string();
    }
    if let Some(n) = val.as_number() {
        if n.fract() == 0.0 && n.abs() < 1e15 {
            return format!("{}", n as i64);
        }
        return format!("{}", n);
    }
    if let Some(b) = val.as_boolean() {
        return if b { "true" } else { "false" }.to_string();
    }
    if val.is_null() {
        return "null".to_string();
    }
    "[object Object]".to_string()
}

/// Helper: set array length
fn set_len(obj: &GcRef<JsObject>, len: usize) {
    obj.set(PropertyKey::string("length"), Value::number(len as f64));
}

/// Create an array iterator object with the given kind ("value", "key", or "entry").
fn make_array_iterator(
    this_val: &Value,
    kind: &str,
    mm: &Arc<MemoryManager>,
    fn_proto: GcRef<JsObject>,
    iter_proto: GcRef<JsObject>,
) -> Result<Value, crate::error::VmError> {
    if this_val.as_object().is_none() && this_val.as_proxy().is_none() {
        return Err("Array iterator: this is not an object".to_string().into());
    }
    let iter = GcRef::new(JsObject::new(Some(iter_proto), mm.clone()));
    // Store the array reference, current index, length, and kind
    iter.set(PropertyKey::string("__array_ref__"), this_val.clone());
    iter.set(PropertyKey::string("__array_index__"), Value::number(0.0));
    iter.set(
        PropertyKey::string("__iter_kind__"),
        Value::string(JsString::intern(kind)),
    );
    // next() method
    let fn_proto_for_next = fn_proto;
    iter.define_property(
        PropertyKey::string("next"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let iter_obj = this_val
                    .as_object()
                    .ok_or_else(|| "not an iterator object".to_string())?;
                let arr_val = iter_obj
                    .get(&PropertyKey::string("__array_ref__"))
                    .ok_or_else(|| "iterator: missing array ref".to_string())?;
                let idx = iter_obj
                    .get(&PropertyKey::string("__array_index__"))
                    .and_then(|v| v.as_number())
                    .unwrap_or(0.0) as usize;
                let len = {
                    let key = PropertyKey::string("length");
                    let len_val = if let Some(proxy) = arr_val.as_proxy() {
                        let key_value = Value::string(JsString::intern("length"));
                        crate::proxy_operations::proxy_get(
                            ncx,
                            proxy,
                            &key,
                            key_value,
                            arr_val.clone(),
                        )?
                    } else if let Some(arr_obj) = arr_val.as_object() {
                        arr_obj.get(&key).unwrap_or(Value::undefined())
                    } else {
                        return Err("iterator: missing array ref".to_string().into());
                    };
                    len_val.as_number().unwrap_or(0.0).max(0.0) as usize
                };
                let kind = iter_obj
                    .get(&PropertyKey::string("__iter_kind__"))
                    .and_then(|v| v.as_string().map(|s| s.as_str().to_string()))
                    .unwrap_or_else(|| "value".to_string());

                if idx >= len {
                    // Done
                    let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                    result.set(PropertyKey::string("value"), Value::undefined());
                    result.set(PropertyKey::string("done"), Value::boolean(true));
                    return Ok(Value::object(result));
                }

                // Advance index
                iter_obj.set(
                    PropertyKey::string("__array_index__"),
                    Value::number((idx + 1) as f64),
                );

                let result = GcRef::new(JsObject::new(None, ncx.memory_manager().clone()));
                match kind.as_str() {
                    "key" => {
                        result.set(PropertyKey::string("value"), Value::number(idx as f64));
                    }
                    "entry" => {
                        let entry = GcRef::new(JsObject::array(2, ncx.memory_manager().clone()));
                        entry.set(PropertyKey::Index(0), Value::number(idx as f64));
                        let val = if let Some(proxy) = arr_val.as_proxy() {
                            let key = PropertyKey::Index(idx as u32);
                            let key_value = Value::string(JsString::intern(&idx.to_string()));
                            crate::proxy_operations::proxy_get(
                                ncx,
                                proxy,
                                &key,
                                key_value,
                                arr_val.clone(),
                            )?
                        } else if let Some(arr_obj) = arr_val.as_object() {
                            arr_obj.get(&PropertyKey::Index(idx as u32)).unwrap_or(Value::undefined())
                        } else {
                            Value::undefined()
                        };
                        entry.set(PropertyKey::Index(1), val);
                        result.set(PropertyKey::string("value"), Value::array(entry));
                    }
                    _ => {
                        // "value"
                        let val = if let Some(proxy) = arr_val.as_proxy() {
                            let key = PropertyKey::Index(idx as u32);
                            let key_value = Value::string(JsString::intern(&idx.to_string()));
                            crate::proxy_operations::proxy_get(
                                ncx,
                                proxy,
                                &key,
                                key_value,
                                arr_val.clone(),
                            )?
                        } else if let Some(arr_obj) = arr_val.as_object() {
                            arr_obj.get(&PropertyKey::Index(idx as u32)).unwrap_or(Value::undefined())
                        } else {
                            Value::undefined()
                        };
                        result.set(PropertyKey::string("value"), val);
                    }
                }
                result.set(PropertyKey::string("done"), Value::boolean(false));
                Ok(Value::object(result))
            },
            mm.clone(),
            fn_proto_for_next,
        )),
    );
    Ok(Value::object(iter))
}

/// Wire all Array.prototype methods to the prototype object
pub fn init_array_prototype(
    arr_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    iterator_proto: GcRef<JsObject>,
    symbol_iterator_id: u64,
) {
        // Array.prototype.push
        arr_proto.define_property(
            PropertyKey::string("push"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.push: this is not an object".to_string())?;
                    let mut len = get_len(&obj);
                    for arg in args {
                        obj.set(PropertyKey::Index(len as u32), arg.clone());
                        len += 1;
                    }
                    set_len(&obj, len);
                    Ok(Value::number(len as f64))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.pop
        arr_proto.define_property(
            PropertyKey::string("pop"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.pop: this is not an object".to_string())?;
                    let len = get_len(&obj);
                    if len == 0 {
                        set_len(&obj, 0);
                        return Ok(Value::undefined());
                    }
                    let idx = PropertyKey::Index((len - 1) as u32);
                    let val = obj.get(&idx).unwrap_or(Value::undefined());
                    obj.delete(&idx);
                    set_len(&obj, len - 1);
                    Ok(val)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.shift
        arr_proto.define_property(
            PropertyKey::string("shift"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.shift: not an object".to_string())?;
                    let len = get_len(&obj);
                    if len == 0 {
                        set_len(&obj, 0);
                        return Ok(Value::undefined());
                    }
                    let first = obj.get(&PropertyKey::Index(0)).unwrap_or(Value::undefined());
                    for i in 1..len {
                        let val = obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        obj.set(PropertyKey::Index((i - 1) as u32), val);
                    }
                    obj.delete(&PropertyKey::Index((len - 1) as u32));
                    set_len(&obj, len - 1);
                    Ok(first)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.unshift
        arr_proto.define_property(
            PropertyKey::string("unshift"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.unshift: not an object".to_string())?;
                    let len = get_len(&obj);
                    let arg_count = args.len();
                    // Shift existing elements right
                    for i in (0..len).rev() {
                        let val = obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        obj.set(PropertyKey::Index((i + arg_count) as u32), val);
                    }
                    // Insert new elements at front
                    for (i, arg) in args.iter().enumerate() {
                        obj.set(PropertyKey::Index(i as u32), arg.clone());
                    }
                    let new_len = len + arg_count;
                    set_len(&obj, new_len);
                    Ok(Value::number(new_len as f64))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.indexOf
        arr_proto.define_property(
            PropertyKey::string("indexOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.indexOf: not an object".to_string())?;
                    let len = get_len(&obj);
                    let search = args.first().cloned().unwrap_or(Value::undefined());
                    let from = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let start = if from < 0 {
                        (len as i64 + from).max(0) as usize
                    } else {
                        from as usize
                    };
                    for i in start..len {
                        if let Some(val) = obj.get(&PropertyKey::Index(i as u32)) {
                            if strict_equal(&val, &search) {
                                return Ok(Value::number(i as f64));
                            }
                        }
                    }
                    Ok(Value::number(-1.0))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.lastIndexOf
        arr_proto.define_property(
            PropertyKey::string("lastIndexOf"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.lastIndexOf: not an object".to_string())?;
                    let len = get_len(&obj);
                    if len == 0 {
                        return Ok(Value::number(-1.0));
                    }
                    let search = args.first().cloned().unwrap_or(Value::undefined());
                    let from = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .unwrap_or((len as f64) - 1.0) as i64;
                    let start = if from < 0 {
                        (len as i64 + from) as usize
                    } else {
                        from.min((len as i64) - 1) as usize
                    };
                    for i in (0..=start).rev() {
                        if let Some(val) = obj.get(&PropertyKey::Index(i as u32)) {
                            if strict_equal(&val, &search) {
                                return Ok(Value::number(i as f64));
                            }
                        }
                    }
                    Ok(Value::number(-1.0))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.includes
        arr_proto.define_property(
            PropertyKey::string("includes"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.includes: not an object".to_string())?;
                    let len = get_len(&obj);
                    let search = args.first().cloned().unwrap_or(Value::undefined());
                    let from = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let start = if from < 0 {
                        (len as i64 + from).max(0) as usize
                    } else {
                        from as usize
                    };
                    for i in start..len {
                        if let Some(val) = obj.get(&PropertyKey::Index(i as u32)) {
                            if same_value_zero(&val, &search) {
                                return Ok(Value::boolean(true));
                            }
                        }
                    }
                    Ok(Value::boolean(false))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.join
        arr_proto.define_property(
            PropertyKey::string("join"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.join: not an object".to_string())?;
                    let len = get_len(&obj);
                    let sep = args
                        .first()
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_string().map(|s| s.as_str().to_string())
                            }
                        })
                        .unwrap_or_else(|| ",".to_string());
                    // Don't pre-allocate with huge lengths (sparse arrays can have
                    // length up to 2^32-1 but only a few actual elements).
                    let mut parts = Vec::with_capacity(len.min(1024));
                    for i in 0..len {
                        let val = obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        if val.is_undefined() || val.is_null() {
                            parts.push(String::new());
                        } else if let Some(s) = val.as_string() {
                            parts.push(s.as_str().to_string());
                        } else if let Some(n) = val.as_number() {
                            if n.fract() == 0.0 && n.abs() < 1e15 {
                                parts.push(format!("{}", n as i64));
                            } else {
                                parts.push(format!("{}", n));
                            }
                        } else if let Some(b) = val.as_boolean() {
                            parts.push(if b { "true" } else { "false" }.to_string());
                        } else {
                            parts.push("[object Object]".to_string());
                        }
                    }
                    Ok(Value::string(JsString::intern(&parts.join(&sep))))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.toString
        arr_proto.define_property(
            PropertyKey::string("toString"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    // toString delegates to join
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.toString: not an object".to_string())?;
                    let len = get_len(&obj);
                    let mut parts = Vec::with_capacity(len.min(1024));
                    for i in 0..len {
                        let val = obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        if val.is_undefined() || val.is_null() {
                            parts.push(String::new());
                        } else if let Some(s) = val.as_string() {
                            parts.push(s.as_str().to_string());
                        } else if let Some(n) = val.as_number() {
                            if n.fract() == 0.0 && n.abs() < 1e15 {
                                parts.push(format!("{}", n as i64));
                            } else {
                                parts.push(format!("{}", n));
                            }
                        } else if let Some(b) = val.as_boolean() {
                            parts.push(if b { "true" } else { "false" }.to_string());
                        } else {
                            parts.push("[object Object]".to_string());
                        }
                    }
                    Ok(Value::string(JsString::intern(&parts.join(","))))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.slice
        arr_proto.define_property(
            PropertyKey::string("slice"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.slice: not an object".to_string())?;
                    let len = get_len(&obj) as i64;
                    let start = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let end = args
                        .get(1)
                        .and_then(|v| {
                            if v.is_undefined() {
                                None
                            } else {
                                v.as_number()
                            }
                        })
                        .unwrap_or(len as f64) as i64;
                    let from = if start < 0 { (len + start).max(0) } else { start.min(len) } as usize;
                    let to = if end < 0 { (len + end).max(0) } else { end.min(len) } as usize;
                    let count = if to > from { to - from } else { 0 };
                    let result = GcRef::new(JsObject::array(count, ncx.memory_manager().clone()));
                    for i in 0..count {
                        let val = obj
                            .get(&PropertyKey::Index((from + i) as u32))
                            .unwrap_or(Value::undefined());
                        result.set(PropertyKey::Index(i as u32), val);
                    }
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.concat
        arr_proto.define_property(
            PropertyKey::string("concat"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let result = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                    let mut idx: u32 = 0;
                    // Copy elements from this
                    if let Some(obj) = this_val.as_object() {
                        let len = get_len(&obj);
                        for i in 0..len {
                            if let Some(val) = obj.get(&PropertyKey::Index(i as u32)) {
                                result.set(PropertyKey::Index(idx), val);
                                idx += 1;
                            }
                        }
                    }
                    // Copy elements from each argument
                    for arg in args {
                        if let Some(arr) = arg.as_object() {
                            // Check if it's an array (has length)
                            if arr.get(&PropertyKey::string("length")).is_some() {
                                let len = get_len(&arr);
                                for i in 0..len {
                                    let val = arr
                                        .get(&PropertyKey::Index(i as u32))
                                        .unwrap_or(Value::undefined());
                                    result.set(PropertyKey::Index(idx), val);
                                    idx += 1;
                                }
                                continue;
                            }
                        }
                        // Non-array: push as single element
                        result.set(PropertyKey::Index(idx), arg.clone());
                        idx += 1;
                    }
                    set_len(&result, idx as usize);
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.reverse
        arr_proto.define_property(
            PropertyKey::string("reverse"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, _args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.reverse: not an object".to_string())?;
                    let len = get_len(&obj);
                    let mut lo = 0usize;
                    let mut hi = if len > 0 { len - 1 } else { 0 };
                    while lo < hi {
                        let lo_val = obj
                            .get(&PropertyKey::Index(lo as u32))
                            .unwrap_or(Value::undefined());
                        let hi_val = obj
                            .get(&PropertyKey::Index(hi as u32))
                            .unwrap_or(Value::undefined());
                        obj.set(PropertyKey::Index(lo as u32), hi_val);
                        obj.set(PropertyKey::Index(hi as u32), lo_val);
                        lo += 1;
                        hi -= 1;
                    }
                    Ok(this_val.clone())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.at
        arr_proto.define_property(
            PropertyKey::string("at"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.at: not an object".to_string())?;
                    let len = get_len(&obj) as i64;
                    let idx = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let actual = if idx < 0 { len + idx } else { idx };
                    if actual < 0 || actual >= len {
                        return Ok(Value::undefined());
                    }
                    Ok(obj
                        .get(&PropertyKey::Index(actual as u32))
                        .unwrap_or(Value::undefined()))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.fill
        arr_proto.define_property(
            PropertyKey::string("fill"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.fill: not an object".to_string())?;
                    let len = get_len(&obj) as i64;
                    let value = args.first().cloned().unwrap_or(Value::undefined());
                    let start = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let end = args
                        .get(2)
                        .and_then(|v| {
                            if v.is_undefined() { None } else { v.as_number() }
                        })
                        .unwrap_or(len as f64) as i64;
                    let from = if start < 0 { (len + start).max(0) } else { start.min(len) } as usize;
                    let to = if end < 0 { (len + end).max(0) } else { end.min(len) } as usize;
                    for i in from..to {
                        obj.set(PropertyKey::Index(i as u32), value.clone());
                    }
                    Ok(this_val.clone())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.splice
        arr_proto.define_property(
            PropertyKey::string("splice"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.splice: not an object".to_string())?;
                    let len = get_len(&obj) as i64;
                    let start_raw = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(0.0) as i64;
                    let actual_start = if start_raw < 0 {
                        (len + start_raw).max(0)
                    } else {
                        start_raw.min(len)
                    } as usize;
                    let delete_count = args
                        .get(1)
                        .and_then(|v| v.as_number())
                        .map(|n| (n as i64).max(0).min(len - actual_start as i64) as usize)
                        .unwrap_or((len - actual_start as i64).max(0) as usize);
                    let items = if args.len() > 2 { &args[2..] } else { &[] };

                    // Collect removed elements
                    let removed = GcRef::new(JsObject::array(delete_count, ncx.memory_manager().clone()));
                    for i in 0..delete_count {
                        let val = obj
                            .get(&PropertyKey::Index((actual_start + i) as u32))
                            .unwrap_or(Value::undefined());
                        removed.set(PropertyKey::Index(i as u32), val);
                    }

                    let item_count = items.len();
                    let ulen = len as usize;

                    if item_count < delete_count {
                        // Shift elements left
                        let diff = delete_count - item_count;
                        for i in actual_start + delete_count..ulen {
                            let val = obj
                                .get(&PropertyKey::Index(i as u32))
                                .unwrap_or(Value::undefined());
                            obj.set(PropertyKey::Index((i - diff) as u32), val);
                        }
                        for i in (ulen - diff)..ulen {
                            obj.delete(&PropertyKey::Index(i as u32));
                        }
                    } else if item_count > delete_count {
                        // Shift elements right
                        let diff = item_count - delete_count;
                        for i in (actual_start + delete_count..ulen).rev() {
                            let val = obj
                                .get(&PropertyKey::Index(i as u32))
                                .unwrap_or(Value::undefined());
                            obj.set(PropertyKey::Index((i + diff) as u32), val);
                        }
                    }

                    // Insert new items
                    for (i, item) in items.iter().enumerate() {
                        obj.set(
                            PropertyKey::Index((actual_start + i) as u32),
                            item.clone(),
                        );
                    }

                    let new_len = ulen - delete_count + item_count;
                    set_len(&obj, new_len);
                    Ok(Value::array(removed))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.flat (depth = 1 by default)
        arr_proto.define_property(
            PropertyKey::string("flat"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| "Array.prototype.flat: not an object".to_string())?;
                    let depth = args
                        .first()
                        .and_then(|v| v.as_number())
                        .unwrap_or(1.0) as i32;

                    fn flatten(
                        source: &GcRef<JsObject>,
                        depth: i32,
                        result: &mut Vec<Value>,
                    ) {
                        let len = get_len(source);
                        for i in 0..len {
                            if let Some(val) = source.get(&PropertyKey::Index(i as u32)) {
                                if depth > 0 {
                                    if let Some(inner) = val.as_object() {
                                        if inner
                                            .get(&PropertyKey::string("length"))
                                            .is_some()
                                        {
                                            flatten(&inner, depth - 1, result);
                                            continue;
                                        }
                                    }
                                }
                                result.push(val);
                            }
                        }
                    }

                    let mut items = Vec::new();
                    flatten(&obj, depth, &mut items);
                    let result_arr =
                        GcRef::new(JsObject::array(items.len(), ncx.memory_manager().clone()));
                    for (i, item) in items.into_iter().enumerate() {
                        result_arr.set(PropertyKey::Index(i as u32), item);
                    }
                    Ok(Value::array(result_arr))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // ================================================================
        // Array callback methods: forEach, map, filter, find, findIndex,
        // every, some, reduce, reduceRight, findLast, findLastIndex,
        // flatMap, sort (with comparator)
        // ================================================================

        // Array.prototype.forEach(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("forEach"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.forEach: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.forEach: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        ncx.call_function(&callback, this_arg.clone(), &[val, Value::number(i as f64), this_val.clone()])?;
                    }
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.map(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("map"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.map: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.map: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    let result = GcRef::new(JsObject::array(len, ncx.memory_manager().clone()));
                    if let Some(array_ctor) = ncx.global().get(&PropertyKey::string("Array")) {
                        if let Some(array_obj) = array_ctor.as_object() {
                            if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                                if let Some(proto_obj) = proto_val.as_object() {
                                    result.set_prototype(Some(proto_obj));
                                }
                            }
                        }
                    }
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let mapped = ncx.call_function(&callback, this_arg.clone(), &[val, Value::number(i as f64), this_val.clone()])?;
                        result.set(PropertyKey::Index(i as u32), mapped);
                    }
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.filter(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("filter"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.filter: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.filter: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    let result = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                    let mut out_idx = 0u32;
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let keep = ncx.call_function(&callback, this_arg.clone(), &[val.clone(), Value::number(i as f64), this_val.clone()])?;
                        if keep.to_boolean() {
                            result.set(PropertyKey::Index(out_idx), val);
                            out_idx += 1;
                        }
                    }
                    set_len(&result, out_idx as usize);
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.find(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("find"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.find: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.find: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let test = ncx.call_function(&callback, this_arg.clone(), &[val.clone(), Value::number(i as f64), this_val.clone()])?;
                        if test.to_boolean() {
                            return Ok(val);
                        }
                    }
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.findIndex(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("findIndex"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.findIndex: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.findIndex: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let test = ncx.call_function(&callback, this_arg.clone(), &[val, Value::number(i as f64), this_val.clone()])?;
                        if test.to_boolean() {
                            return Ok(Value::number(i as f64));
                        }
                    }
                    Ok(Value::number(-1.0))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.findLast(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("findLast"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.findLast: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.findLast: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    for i in (0..len).rev() {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let test = ncx.call_function(&callback, this_arg.clone(), &[val.clone(), Value::number(i as f64), this_val.clone()])?;
                        if test.to_boolean() {
                            return Ok(val);
                        }
                    }
                    Ok(Value::undefined())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.findLastIndex(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("findLastIndex"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.findLastIndex: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.findLastIndex: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    for i in (0..len).rev() {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let test = ncx.call_function(&callback, this_arg.clone(), &[val, Value::number(i as f64), this_val.clone()])?;
                        if test.to_boolean() {
                            return Ok(Value::number(i as f64));
                        }
                    }
                    Ok(Value::number(-1.0))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.every(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("every"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.every: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.every: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let test = ncx.call_function(&callback, this_arg.clone(), &[val, Value::number(i as f64), this_val.clone()])?;
                        if !test.to_boolean() {
                            return Ok(Value::boolean(false));
                        }
                    }
                    Ok(Value::boolean(true))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.some(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("some"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.some: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.some: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let test = ncx.call_function(&callback, this_arg.clone(), &[val, Value::number(i as f64), this_val.clone()])?;
                        if test.to_boolean() {
                            return Ok(Value::boolean(true));
                        }
                    }
                    Ok(Value::boolean(false))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.reduce(callback [, initialValue])
        arr_proto.define_property(
            PropertyKey::string("reduce"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.reduce: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.reduce: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    let has_initial = args.len() > 1;
                    let mut accumulator = if has_initial {
                        args[1].clone()
                    } else {
                        if len == 0 {
                            return Err(VmError::type_error("Reduce of empty array with no initial value"));
                        }
                        obj.get(&PropertyKey::Index(0)).unwrap_or(Value::undefined())
                    };
                    let start = if has_initial { 0 } else { 1 };
                    for i in start..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        accumulator = ncx.call_function(
                            &callback,
                            Value::undefined(),
                            &[accumulator, val, Value::number(i as f64), this_val.clone()],
                        )?;
                    }
                    Ok(accumulator)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.reduceRight(callback [, initialValue])
        arr_proto.define_property(
            PropertyKey::string("reduceRight"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.reduceRight: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.reduceRight: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    let has_initial = args.len() > 1;
                    let mut accumulator = if has_initial {
                        args[1].clone()
                    } else {
                        if len == 0 {
                            return Err(VmError::type_error("Reduce of empty array with no initial value"));
                        }
                        obj.get(&PropertyKey::Index((len - 1) as u32)).unwrap_or(Value::undefined())
                    };
                    let end = if has_initial { len } else { len.saturating_sub(1) };
                    for i in (0..end).rev() {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        accumulator = ncx.call_function(
                            &callback,
                            Value::undefined(),
                            &[accumulator, val, Value::number(i as f64), this_val.clone()],
                        )?;
                    }
                    Ok(accumulator)
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.flatMap(callback [, thisArg])
        arr_proto.define_property(
            PropertyKey::string("flatMap"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.flatMap: this is not an object"))?;
                    let callback = args.first().cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                    if !callback.is_callable() {
                        return Err(VmError::type_error("Array.prototype.flatMap: callback is not a function"));
                    }
                    let len = get_len(&obj);
                    let result = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                    let mut out_idx = 0u32;
                    for i in 0..len {
                        let val = obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined());
                        let mapped = ncx.call_function(&callback, this_arg.clone(), &[val, Value::number(i as f64), this_val.clone()])?;
                        // Flatten one level
                        if let Some(inner) = mapped.as_object() {
                            if inner.get(&PropertyKey::string("length")).is_some() {
                                let inner_len = get_len(&inner);
                                for j in 0..inner_len {
                                    let item = inner.get(&PropertyKey::Index(j as u32)).unwrap_or(Value::undefined());
                                    result.set(PropertyKey::Index(out_idx), item);
                                    out_idx += 1;
                                }
                                continue;
                            }
                        }
                        result.set(PropertyKey::Index(out_idx), mapped);
                        out_idx += 1;
                    }
                    set_len(&result, out_idx as usize);
                    Ok(Value::array(result))
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.sort([compareFn])
        arr_proto.define_property(
            PropertyKey::string("sort"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.sort: this is not an object"))?;
                    let compare_fn = args.first().cloned().unwrap_or(Value::undefined());
                    let len = get_len(&obj);

                    // Collect elements
                    let mut elements: Vec<Value> = Vec::with_capacity(len.min(1024));
                    for i in 0..len {
                        elements.push(obj.get(&PropertyKey::Index(i as u32)).unwrap_or(Value::undefined()));
                    }

                    if compare_fn.is_undefined() {
                        // Default: sort by toString
                        elements.sort_by(|a, b| {
                            let sa = value_to_sort_string(a);
                            let sb = value_to_sort_string(b);
                            sa.cmp(&sb)
                        });
                    } else if compare_fn.is_callable() {
                        // Custom comparator (closure or native function)
                        let mut err: Option<VmError> = None;
                        elements.sort_by(|a, b| {
                            if err.is_some() {
                                return std::cmp::Ordering::Equal;
                            }
                            match ncx.call_function(&compare_fn, Value::undefined(), &[a.clone(), b.clone()]) {
                                Ok(result) => {
                                    let n = result.as_number().unwrap_or(0.0);
                                    if n < 0.0 {
                                        std::cmp::Ordering::Less
                                    } else if n > 0.0 {
                                        std::cmp::Ordering::Greater
                                    } else {
                                        std::cmp::Ordering::Equal
                                    }
                                }
                                Err(e) => {
                                    err = Some(e);
                                    std::cmp::Ordering::Equal
                                }
                            }
                        });
                        if let Some(e) = err {
                            return Err(e);
                        }
                    } else {
                        return Err(VmError::type_error("Array.prototype.sort: comparator is not a function"));
                    }

                    // Write sorted elements back
                    for (i, val) in elements.into_iter().enumerate() {
                        obj.set(PropertyKey::Index(i as u32), val);
                    }
                    Ok(this_val.clone())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // Array.prototype.copyWithin(target, start [, end])
        arr_proto.define_property(
            PropertyKey::string("copyWithin"),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                |this_val, args, _ncx| {
                    let obj = this_val
                        .as_object()
                        .ok_or_else(|| VmError::type_error("Array.prototype.copyWithin: this is not an object"))?;
                    let len = get_len(&obj) as i64;
                    let target = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                    let start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                    let end = args.get(2).and_then(|v| {
                        if v.is_undefined() { None } else { v.as_number() }
                    }).unwrap_or(len as f64) as i64;

                    let to = if target < 0 { (len + target).max(0) } else { target.min(len) } as usize;
                    let from = if start < 0 { (len + start).max(0) } else { start.min(len) } as usize;
                    let fin = if end < 0 { (len + end).max(0) } else { end.min(len) } as usize;
                    let count = (fin.saturating_sub(from)).min((len as usize).saturating_sub(to));

                    // Copy in correct direction to handle overlapping
                    if from < to && to < from + count {
                        for i in (0..count).rev() {
                            let val = obj.get(&PropertyKey::Index((from + i) as u32)).unwrap_or(Value::undefined());
                            obj.set(PropertyKey::Index((to + i) as u32), val);
                        }
                    } else {
                        for i in 0..count {
                            let val = obj.get(&PropertyKey::Index((from + i) as u32)).unwrap_or(Value::undefined());
                            obj.set(PropertyKey::Index((to + i) as u32), val);
                        }
                    }
                    Ok(this_val.clone())
                },
                mm.clone(),
                fn_proto,
            )),
        );

        // ================================================================
        // Array.prototype.values / keys / entries / [Symbol.iterator]
        // ================================================================
        {
            let iter_proto = iterator_proto;
            let sym_id = symbol_iterator_id;

            // Array.prototype.values()
            let fn_p = fn_proto;
            let ip = iter_proto;
            arr_proto.define_property(
                PropertyKey::string("values"),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    move |this_val, _args, ncx| {
                        make_array_iterator(this_val, "value", ncx.memory_manager(), fn_p, ip)
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );

            // Array.prototype.keys()
            let fn_p = fn_proto;
            let ip = iter_proto;
            arr_proto.define_property(
                PropertyKey::string("keys"),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    move |this_val, _args, ncx| {
                        make_array_iterator(this_val, "key", ncx.memory_manager(), fn_p, ip)
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );

            // Array.prototype.entries()
            let fn_p = fn_proto;
            let ip = iter_proto;
            arr_proto.define_property(
                PropertyKey::string("entries"),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    move |this_val, _args, ncx| {
                        make_array_iterator(this_val, "entry", ncx.memory_manager(), fn_p, ip)
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );

            // Array.prototype[Symbol.iterator] = Array.prototype.values
            let fn_p = fn_proto;
            let ip = iter_proto;
            arr_proto.define_property(
                PropertyKey::Symbol(sym_id),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    move |this_val, _args, ncx| {
                        make_array_iterator(this_val, "value", ncx.memory_manager(), fn_p, ip)
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );
        }
}

/// Install static methods on the Array constructor.
pub fn install_array_statics(
    ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Array.isArray(arg) — §23.1.2.2
    ctor.define_property(
        PropertyKey::string("isArray"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, _ncx| {
                let is_arr = args
                    .first()
                    .and_then(|v| v.as_object())
                    .map(|o| o.is_array())
                    .unwrap_or(false);
                Ok(Value::boolean(is_arr))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Array.from(arrayLike) — §23.1.2.1 (simplified)
    ctor.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let source = args
                    .first()
                    .ok_or_else(|| "Array.from requires an argument".to_string())?;
                if let Some(obj) = source.as_object() {
                    if let Some(len_val) = obj.get(&PropertyKey::string("length")) {
                        let len = len_val.as_number().unwrap_or(0.0) as usize;
                        let result = GcRef::new(JsObject::array(len, ncx.memory_manager().clone()));
                        for i in 0..len {
                            let val = obj
                                .get(&PropertyKey::Index(i as u32))
                                .unwrap_or(Value::undefined());
                            result.set(PropertyKey::Index(i as u32), val);
                        }
                        return Ok(Value::array(result));
                    }
                }
                Ok(Value::array(GcRef::new(JsObject::array(0, ncx.memory_manager().clone()))))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Array.of(...items) — §23.1.2.3
    ctor.define_property(
        PropertyKey::string("of"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let result = GcRef::new(JsObject::array(args.len(), ncx.memory_manager().clone()));
                for (i, arg) in args.iter().enumerate() {
                    result.set(PropertyKey::Index(i as u32), arg.clone());
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}
