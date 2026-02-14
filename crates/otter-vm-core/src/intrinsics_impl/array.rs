//! Array constructor statics and prototype methods (ES2026)
//!
//! ## Constructor statics:
//! - `Array.isArray()`, `Array.from()`, `Array.of()`
//!
//! ## Prototype methods:
//! - push, pop, shift, unshift, indexOf, lastIndexOf, includes, join, toString,
//!   slice, concat, reverse, at, fill, splice, flat, forEach, map, filter,
//!   reduce, reduceRight, find, findIndex, every, some, sort, entries, keys, values, copyWithin

use crate::context::NativeContext;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::intrinsics_impl::helpers::{same_value_zero, strict_equal};
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::value::Value;
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
        return crate::globals::js_number_to_string(n);
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
    let _ = obj.set(PropertyKey::string("length"), Value::number(len as f64));
}

/// Helper: get a property value from an object, invoking getters if it's an accessor.
/// This is the spec-compliant [[Get]] that calls accessor getters via NativeContext.
fn js_get(
    obj: &GcRef<JsObject>,
    key: &PropertyKey,
    ncx: &mut NativeContext<'_>,
) -> Result<Value, VmError> {
    use crate::object::PropertyDescriptor;
    let desc_opt = obj.lookup_property_descriptor(key);
    if let Some(desc) = desc_opt {
        match desc {
            PropertyDescriptor::Data { value, .. } => Ok(value),
            PropertyDescriptor::Accessor { get, .. } => {
                if let Some(getter) = get {
                    if !getter.is_undefined() {
                        return ncx.call_function(&getter, Value::object(obj.clone()), &[]);
                    }
                }
                Ok(Value::undefined())
            }
            PropertyDescriptor::Deleted => Ok(Value::undefined()),
        }
    } else {
        Ok(Value::undefined())
    }
}

/// Create a new array with the correct Array.prototype from the global.
fn create_default_array(length: usize, ncx: &mut NativeContext<'_>) -> GcRef<JsObject> {
    let arr = GcRef::new(JsObject::array(length, ncx.memory_manager().clone()));
    // Set Array.prototype so methods like .map(), .filter() work on the result
    if let Some(array_ctor) = ncx.global().get(&PropertyKey::string("Array")) {
        if let Some(array_obj) = array_ctor
            .as_object()
            .or_else(|| array_ctor.native_function_object())
        {
            if let Some(proto_val) = array_obj.get(&PropertyKey::string("prototype")) {
                if let Some(proto_obj) = proto_val.as_object() {
                    arr.set_prototype(Value::object(proto_obj));
                }
            }
        }
    }
    arr
}

/// ArraySpeciesCreate(originalArray, length) — ES2024 §9.4.2.3
/// Creates a new array using the species constructor of the original array,
/// or falls back to the default Array constructor.
fn array_species_create(
    original_array: &GcRef<JsObject>,
    length: usize,
    ncx: &mut NativeContext<'_>,
) -> Result<GcRef<JsObject>, VmError> {
    // 2. If IsArray(originalArray) is false, return ArrayCreate(length)
    if !original_array.is_array() {
        return Ok(create_default_array(length, ncx));
    }
    // 3. Let C = Get(originalArray, "constructor")
    let c = original_array
        .get(&PropertyKey::string("constructor"))
        .unwrap_or(Value::undefined());
    // 4. If C is undefined, return ArrayCreate(length)
    if c.is_undefined() {
        return Ok(create_default_array(length, ncx));
    }
    // 5. If Type(C) is Object, let S = Get(C, @@species)
    if let Some(c_obj) = c.as_object().or_else(|| c.native_function_object()) {
        let species_symbol = crate::intrinsics::well_known::species_symbol();
        let species_key = PropertyKey::Symbol(species_symbol);
        // Use lookup_property_descriptor to handle accessor (getter) for @@species
        let s = if let Some(desc) = c_obj.lookup_property_descriptor(&species_key) {
            match desc {
                PropertyDescriptor::Data { value, .. } => value,
                PropertyDescriptor::Accessor { get, .. } => {
                    if let Some(getter) = get {
                        if !getter.is_undefined() {
                            ncx.call_function(&getter, c.clone(), &[])?
                        } else {
                            Value::undefined()
                        }
                    } else {
                        Value::undefined()
                    }
                }
                PropertyDescriptor::Deleted => Value::undefined(),
            }
        } else {
            Value::undefined()
        };
        // 6. If S is undefined or null, return ArrayCreate(length)
        if s.is_undefined() || s.is_null() {
            return Ok(create_default_array(length, ncx));
        }
        // 7. If IsConstructor(S), return Construct(S, [length])
        if s.is_callable() {
            let result = ncx.call_function_construct(
                &s,
                Value::undefined(),
                &[Value::number(length as f64)],
            )?;
            if let Some(obj) = result.as_object() {
                return Ok(obj);
            }
            return Err(VmError::type_error(
                "Species constructor did not return an object",
            ));
        }
        // 8. Throw a TypeError
        return Err(VmError::type_error(
            "Species constructor is not a constructor",
        ));
    }
    // C is not an object — if it's callable (e.g. bound function), use as constructor
    if c.is_callable() {
        let result =
            ncx.call_function_construct(&c, Value::undefined(), &[Value::number(length as f64)])?;
        return result
            .as_object()
            .ok_or_else(|| VmError::type_error("Constructor did not return an object"));
    }
    // C is not an object and not callable — throw TypeError
    Err(VmError::type_error("Constructor is not a constructor"))
}

/// Create an array iterator object with the given kind ("value", "key", or "entry").
fn make_array_iterator(
    this_val: &Value,
    kind: &str,
    mm: &Arc<MemoryManager>,
    _fn_proto: GcRef<JsObject>,
    array_iter_proto: GcRef<JsObject>,
) -> Result<Value, crate::error::VmError> {
    if this_val.as_object().is_none() && this_val.as_proxy().is_none() {
        return Err("Array iterator: this is not an object".to_string().into());
    }
    // Create iterator with %ArrayIteratorPrototype% as prototype (has next() on it)
    let iter = GcRef::new(JsObject::new(Value::object(array_iter_proto), mm.clone()));
    // Store the array reference, current index, and kind
    let _ = iter.set(PropertyKey::string("__array_ref__"), this_val.clone());
    let _ = iter.set(PropertyKey::string("__array_index__"), Value::number(0.0));
    let _ = iter.set(
        PropertyKey::string("__iter_kind__"),
        Value::string(JsString::intern(kind)),
    );
    Ok(Value::object(iter))
}

/// Wire all Array.prototype methods to the prototype object
pub fn init_array_prototype(
    arr_proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    array_iterator_proto: GcRef<JsObject>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
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
                    let _ = obj.set(PropertyKey::Index(len as u32), arg.clone());
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
                let first = obj
                    .get(&PropertyKey::Index(0))
                    .unwrap_or(Value::undefined());
                for i in 1..len {
                    let val = obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::undefined());
                    let _ = obj.set(PropertyKey::Index((i - 1) as u32), val);
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
            |this_val, args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| "Array.prototype.unshift: not an object".to_string())?;
                let len = get_len(&obj);
                let arg_count = args.len();
                // Shift existing elements right
                for i in (0..len).rev() {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::undefined());
                    let _ = obj.set(PropertyKey::Index((i + arg_count) as u32), val);
                }
                // Insert new elements at front
                for (i, arg) in args.iter().enumerate() {
                    let _ = obj.set(PropertyKey::Index(i as u32), arg.clone());
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
            |this_val, args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| "Array.prototype.indexOf: not an object".to_string())?;
                let len = get_len(&obj);
                let search = args.first().cloned().unwrap_or(Value::undefined());
                let from = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                let start = if from < 0 {
                    (len as i64 + from).max(0) as usize
                } else {
                    from as usize
                };
                for i in start..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
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
            |this_val, args, ncx| {
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
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
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
            |this_val, args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| "Array.prototype.includes: not an object".to_string())?;
                let len = get_len(&obj);
                let search = args.first().cloned().unwrap_or(Value::undefined());
                let from = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                let start = if from < 0 {
                    (len as i64 + from).max(0) as usize
                } else {
                    from as usize
                };
                for i in start..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Note: includes does NOT skip holes when searching for undefined
                    // Per ES2023 §23.1.3.16, includes uses Get which returns undefined
                    // for holes, then SameValueZero(undefined, searchElement).
                    let val = obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::undefined());
                    if same_value_zero(&val, &search) {
                        return Ok(Value::boolean(true));
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
            |this_val, args, ncx| {
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
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::undefined());
                    if val.is_undefined() || val.is_null() {
                        parts.push(String::new());
                    } else {
                        parts.push(ncx.to_string_value(&val)?);
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
            |this_val, _args, ncx| {
                // toString delegates to join
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| "Array.prototype.toString: not an object".to_string())?;
                let len = get_len(&obj);
                let mut parts = Vec::with_capacity(len.min(1024));
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = obj
                        .get(&PropertyKey::Index(i as u32))
                        .unwrap_or(Value::undefined());
                    if val.is_undefined() || val.is_null() {
                        parts.push(String::new());
                    } else {
                        parts.push(ncx.to_string_value(&val)?);
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
                let start = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
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
                let from = if start < 0 {
                    (len + start).max(0)
                } else {
                    start.min(len)
                } as usize;
                let to = if end < 0 {
                    (len + end).max(0)
                } else {
                    end.min(len)
                } as usize;
                let count = if to > from { to - from } else { 0 };
                let result = array_species_create(&obj, count, ncx)?;
                for i in 0..count {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, preserve holes: only set if present in source
                    if obj.has(&PropertyKey::Index((from + i) as u32)) {
                        let val = obj
                            .get(&PropertyKey::Index((from + i) as u32))
                            .unwrap_or(Value::undefined());
                        let _ = result.set(PropertyKey::Index(i as u32), val);
                    }
                }
                set_len(&result, count);
                Ok(if result.is_array() {
                    Value::array(result)
                } else {
                    Value::object(result)
                })
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
                let this_obj = this_val
                    .as_object()
                    .ok_or_else(|| "Array.prototype.concat: this is not an object".to_string())?;
                let result = array_species_create(&this_obj, 0, ncx)?;
                let mut idx: u32 = 0;
                // Copy elements from this (preserve holes)
                {
                    let len = get_len(&this_obj);
                    for i in 0..len {
                        if i & 0x3FF == 0 {
                            ncx.check_for_interrupt()?;
                        }
                        if this_obj.has(&PropertyKey::Index(i as u32)) {
                            let val = js_get(&this_obj, &PropertyKey::Index(i as u32), ncx)?;
                            let _ = result.set(PropertyKey::Index(idx), val);
                        }
                        // else: hole — leave result[idx] as hole
                        idx += 1;
                    }
                }
                // Copy elements from each argument
                for arg in args {
                    if let Some(arr) = arg.as_object() {
                        // Check if it's an array (has length)
                        if arr.get(&PropertyKey::string("length")).is_some() {
                            let len = get_len(&arr);
                            for i in 0..len {
                                if i & 0x3FF == 0 {
                                    ncx.check_for_interrupt()?;
                                }
                                if arr.has(&PropertyKey::Index(i as u32)) {
                                    let val = arr
                                        .get(&PropertyKey::Index(i as u32))
                                        .unwrap_or(Value::undefined());
                                    let _ = result.set(PropertyKey::Index(idx), val);
                                }
                                idx += 1;
                            }
                            continue;
                        }
                    }
                    // Non-array: push as single element
                    let _ = result.set(PropertyKey::Index(idx), arg.clone());
                    idx += 1;
                }
                set_len(&result, idx as usize);
                Ok(if result.is_array() {
                    Value::array(result)
                } else {
                    Value::object(result)
                })
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Array.prototype.reverse
    arr_proto.define_property(
        PropertyKey::string("reverse"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| "Array.prototype.reverse: not an object".to_string())?;
                let len = get_len(&obj);
                let mut lo = 0usize;
                let mut hi = if len > 0 { len - 1 } else { 0 };
                while lo < hi {
                    if lo & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let lo_val = obj
                        .get(&PropertyKey::Index(lo as u32))
                        .unwrap_or(Value::undefined());
                    let hi_val = obj
                        .get(&PropertyKey::Index(hi as u32))
                        .unwrap_or(Value::undefined());
                    let _ = obj.set(PropertyKey::Index(lo as u32), hi_val);
                    let _ = obj.set(PropertyKey::Index(hi as u32), lo_val);
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
                let idx = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
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
            |this_val, args, ncx| {
                let obj = this_val
                    .as_object()
                    .ok_or_else(|| "Array.prototype.fill: not an object".to_string())?;
                let len = get_len(&obj) as i64;
                let value = args.first().cloned().unwrap_or(Value::undefined());
                let start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                let end = args
                    .get(2)
                    .and_then(|v| {
                        if v.is_undefined() {
                            None
                        } else {
                            v.as_number()
                        }
                    })
                    .unwrap_or(len as f64) as i64;
                let from = if start < 0 {
                    (len + start).max(0)
                } else {
                    start.min(len)
                } as usize;
                let to = if end < 0 {
                    (len + end).max(0)
                } else {
                    end.min(len)
                } as usize;
                for i in from..to {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let _ = obj.set(PropertyKey::Index(i as u32), value.clone());
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
                let start_raw = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
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
                let removed = array_species_create(&obj, delete_count, ncx)?;
                for i in 0..delete_count {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = obj
                        .get(&PropertyKey::Index((actual_start + i) as u32))
                        .unwrap_or(Value::undefined());
                    let _ = removed.set(PropertyKey::Index(i as u32), val);
                }
                set_len(&removed, delete_count);

                let item_count = items.len();
                let ulen = len as usize;

                if item_count < delete_count {
                    // Shift elements left
                    let diff = delete_count - item_count;
                    for i in actual_start + delete_count..ulen {
                        if i & 0x3FF == 0 {
                            ncx.check_for_interrupt()?;
                        }
                        let val = obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        let _ = obj.set(PropertyKey::Index((i - diff) as u32), val);
                    }
                    for i in (ulen - diff)..ulen {
                        obj.delete(&PropertyKey::Index(i as u32));
                    }
                } else if item_count > delete_count {
                    // Shift elements right
                    let diff = item_count - delete_count;
                    for i in (actual_start + delete_count..ulen).rev() {
                        if i & 0x3FF == 0 {
                            ncx.check_for_interrupt()?;
                        }
                        let val = obj
                            .get(&PropertyKey::Index(i as u32))
                            .unwrap_or(Value::undefined());
                        let _ = obj.set(PropertyKey::Index((i + diff) as u32), val);
                    }
                }

                // Insert new items
                for (i, item) in items.iter().enumerate() {
                    let _ = obj.set(PropertyKey::Index((actual_start + i) as u32), item.clone());
                }

                let new_len = ulen - delete_count + item_count;
                set_len(&obj, new_len);
                Ok(if removed.is_array() {
                    Value::array(removed)
                } else {
                    Value::object(removed)
                })
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
                let depth = args.first().and_then(|v| v.as_number()).unwrap_or(1.0) as i32;

                fn flatten(source: &GcRef<JsObject>, depth: i32, result: &mut Vec<Value>) {
                    let len = get_len(source);
                    for i in 0..len {
                        // Per spec, skip holes
                        if !source.has(&PropertyKey::Index(i as u32)) {
                            continue;
                        }
                        if let Some(val) = source.get(&PropertyKey::Index(i as u32)) {
                            if depth > 0 {
                                if let Some(inner) = val.as_object() {
                                    if inner.get(&PropertyKey::string("length")).is_some() {
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
                let items_len = items.len();
                let result_arr = array_species_create(&obj, 0, ncx)?;
                for (i, item) in items.into_iter().enumerate() {
                    let _ = result_arr.set(PropertyKey::Index(i as u32), item);
                }
                set_len(&result_arr, items_len);
                Ok(if result_arr.is_array() {
                    Value::array(result_arr)
                } else {
                    Value::object(result_arr)
                })
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.forEach: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.forEach: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes (absent elements)
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.map: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.map: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                let result = array_species_create(&obj, len, ncx)?;
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes (absent elements) — hole stays in result
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let mapped = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
                    let _ = result.set(PropertyKey::Index(i as u32), mapped);
                }
                Ok(if result.is_array() {
                    Value::array(result)
                } else {
                    Value::object(result)
                })
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.filter: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.filter: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                let result = array_species_create(&obj, 0, ncx)?;
                let mut out_idx = 0u32;
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let keep = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val.clone(), Value::number(i as f64), this_val.clone()],
                    )?;
                    if keep.to_boolean() {
                        let _ = result.set(PropertyKey::Index(out_idx), val);
                        out_idx += 1;
                    }
                }
                set_len(&result, out_idx as usize);
                Ok(if result.is_array() {
                    Value::array(result)
                } else {
                    Value::object(result)
                })
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.find: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.find: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let test = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val.clone(), Value::number(i as f64), this_val.clone()],
                    )?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.findIndex: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.findIndex: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let test = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.findLast: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.findLast: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                for i in (0..len).rev() {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let test = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val.clone(), Value::number(i as f64), this_val.clone()],
                    )?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.findLastIndex: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.findLastIndex: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                for i in (0..len).rev() {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let test = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.every: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.every: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let test = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.some: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.some: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let test = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.reduce: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.reduce: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                let has_initial = args.len() > 1;
                let mut accumulator;
                let mut start;
                if has_initial {
                    accumulator = args[1].clone();
                    start = 0;
                } else {
                    // Find first present element for initial value
                    start = 0;
                    accumulator = Value::undefined();
                    let mut found = false;
                    for i in 0..len {
                        if i & 0x3FF == 0 {
                            ncx.check_for_interrupt()?;
                        }
                        if obj.has(&PropertyKey::Index(i as u32)) {
                            accumulator = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                            start = i + 1;
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return Err(VmError::type_error(
                            "Reduce of empty array with no initial value",
                        ));
                    }
                }
                for i in start..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.reduceRight: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.reduceRight: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                let has_initial = args.len() > 1;
                let mut accumulator;
                let mut end;
                if has_initial {
                    accumulator = args[1].clone();
                    end = len;
                } else {
                    // Find last present element for initial value
                    end = 0;
                    accumulator = Value::undefined();
                    let mut found = false;
                    for i in (0..len).rev() {
                        if i & 0x3FF == 0 {
                            ncx.check_for_interrupt()?;
                        }
                        if obj.has(&PropertyKey::Index(i as u32)) {
                            accumulator = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                            end = i;
                            found = true;
                            break;
                        }
                    }
                    if !found {
                        return Err(VmError::type_error(
                            "Reduce of empty array with no initial value",
                        ));
                    }
                }
                for i in (0..end).rev() {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.flatMap: this is not an object")
                })?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());

                if !callback.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.flatMap: callback is not a function",
                    ));
                }
                let len = get_len(&obj);
                let result = array_species_create(&obj, 0, ncx)?;
                let mut out_idx = 0u32;
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    // Per spec, skip holes
                    if !obj.has(&PropertyKey::Index(i as u32)) {
                        continue;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let mapped = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
                    // Flatten one level
                    if let Some(inner) = mapped.as_object() {
                        if inner.get(&PropertyKey::string("length")).is_some() {
                            let inner_len = get_len(&inner);
                            for j in 0..inner_len {
                                let item = js_get(&inner, &PropertyKey::Index(j as u32), ncx)?;
                                let _ = result.set(PropertyKey::Index(out_idx), item);
                                out_idx += 1;
                            }
                            continue;
                        }
                    }
                    let _ = result.set(PropertyKey::Index(out_idx), mapped);
                    out_idx += 1;
                }
                set_len(&result, out_idx as usize);
                Ok(if result.is_array() {
                    Value::array(result)
                } else {
                    Value::object(result)
                })
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
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.sort: this is not an object")
                })?;
                let compare_fn = args.first().cloned().unwrap_or(Value::undefined());
                let len = get_len(&obj);

                // Collect elements
                let mut elements: Vec<Value> = Vec::with_capacity(len.min(1024));
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    elements.push(js_get(&obj, &PropertyKey::Index(i as u32), ncx)?);
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
                        match ncx.call_function(
                            &compare_fn,
                            Value::undefined(),
                            &[a.clone(), b.clone()],
                        ) {
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
                    return Err(VmError::type_error(
                        "Array.prototype.sort: comparator is not a function",
                    ));
                }

                // Write sorted elements back
                for (i, val) in elements.into_iter().enumerate() {
                    let _ = obj.set(PropertyKey::Index(i as u32), val);
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
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.copyWithin: this is not an object")
                })?;
                let len = get_len(&obj) as i64;
                let target = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                let start = args.get(1).and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                let end = args
                    .get(2)
                    .and_then(|v| {
                        if v.is_undefined() {
                            None
                        } else {
                            v.as_number()
                        }
                    })
                    .unwrap_or(len as f64) as i64;

                let to = if target < 0 {
                    (len + target).max(0)
                } else {
                    target.min(len)
                } as usize;
                let from = if start < 0 {
                    (len + start).max(0)
                } else {
                    start.min(len)
                } as usize;
                let fin = if end < 0 {
                    (len + end).max(0)
                } else {
                    end.min(len)
                } as usize;
                let count = (fin.saturating_sub(from)).min((len as usize).saturating_sub(to));

                // Copy in correct direction to handle overlapping
                if from < to && to < from + count {
                    for i in (0..count).rev() {
                        if i & 0x3FF == 0 {
                            ncx.check_for_interrupt()?;
                        }
                        let val = obj
                            .get(&PropertyKey::Index((from + i) as u32))
                            .unwrap_or(Value::undefined());
                        let _ = obj.set(PropertyKey::Index((to + i) as u32), val);
                    }
                } else {
                    for i in 0..count {
                        if i & 0x3FF == 0 {
                            ncx.check_for_interrupt()?;
                        }
                        let val = obj
                            .get(&PropertyKey::Index((from + i) as u32))
                            .unwrap_or(Value::undefined());
                        let _ = obj.set(PropertyKey::Index((to + i) as u32), val);
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
        let iter_proto = array_iterator_proto;
        let sym_ref = symbol_iterator;

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
            PropertyKey::Symbol(sym_ref),
            PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                move |this_val, _args, ncx| {
                    make_array_iterator(this_val, "value", ncx.memory_manager(), fn_p, ip)
                },
                mm.clone(),
                fn_proto,
            )),
        );
    }

    // ================================================================
    // ES2023 Change Array by Copy methods
    // ================================================================

    // Array.prototype.toReversed() — §23.1.3.33
    arr_proto.define_property(
        PropertyKey::string("toReversed"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.toReversed: this is not an object")
                })?;
                let len = get_len(&obj);
                let result = GcRef::new(JsObject::array(len, ncx.memory_manager().clone()));
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let from_idx = len - 1 - i;
                    let val = js_get(&obj, &PropertyKey::Index(from_idx as u32), ncx)?;
                    let _ = result.set(PropertyKey::Index(i as u32), val);
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Array.prototype.toSorted([compareFn]) — §23.1.3.34
    arr_proto.define_property(
        PropertyKey::string("toSorted"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.toSorted: this is not an object")
                })?;
                let compare_fn = args.first().cloned().unwrap_or(Value::undefined());
                if !compare_fn.is_undefined() && !compare_fn.is_callable() {
                    return Err(VmError::type_error(
                        "Array.prototype.toSorted: comparator is not a function",
                    ));
                }
                let len = get_len(&obj);

                // Collect elements
                let mut elements: Vec<Value> = Vec::with_capacity(len.min(1024));
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    elements.push(js_get(&obj, &PropertyKey::Index(i as u32), ncx)?);
                }

                if compare_fn.is_undefined() {
                    elements.sort_by(|a, b| {
                        let sa = value_to_sort_string(a);
                        let sb = value_to_sort_string(b);
                        sa.cmp(&sb)
                    });
                } else {
                    let mut err: Option<VmError> = None;
                    elements.sort_by(|a, b| {
                        if err.is_some() {
                            return std::cmp::Ordering::Equal;
                        }
                        match ncx.call_function(
                            &compare_fn,
                            Value::undefined(),
                            &[a.clone(), b.clone()],
                        ) {
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
                }

                let result = GcRef::new(JsObject::array(len, ncx.memory_manager().clone()));
                for (i, val) in elements.into_iter().enumerate() {
                    let _ = result.set(PropertyKey::Index(i as u32), val);
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Array.prototype.toSpliced(start, deleteCount, ...items) — §23.1.3.35
    arr_proto.define_property(
        PropertyKey::string("toSpliced"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.toSpliced: this is not an object")
                })?;
                let len = get_len(&obj) as i64;
                let start_raw = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                let actual_start = if start_raw < 0 {
                    (len + start_raw).max(0)
                } else {
                    start_raw.min(len)
                } as usize;

                let insert_count = if args.len() > 2 { args.len() - 2 } else { 0 };
                let delete_count = if args.len() == 0 {
                    0
                } else if args.len() == 1 {
                    (len as usize) - actual_start
                } else {
                    args.get(1)
                        .and_then(|v| v.as_number())
                        .map(|n| (n as i64).max(0).min(len - actual_start as i64) as usize)
                        .unwrap_or(0)
                };

                let new_len = (len as usize) - delete_count + insert_count;
                let result = GcRef::new(JsObject::array(new_len, ncx.memory_manager().clone()));
                let mut r: u32 = 0;

                // Copy elements before start
                for i in 0..actual_start {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let _ = result.set(PropertyKey::Index(r), val);
                    r += 1;
                }
                // Insert new items
                if args.len() > 2 {
                    for item in &args[2..] {
                        let _ = result.set(PropertyKey::Index(r), item.clone());
                        r += 1;
                    }
                }
                // Copy elements after deleted section
                for i in (actual_start + delete_count)..(len as usize) {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                    let _ = result.set(PropertyKey::Index(r), val);
                    r += 1;
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // Array.prototype.with(index, value) — §23.1.3.39
    arr_proto.define_property(
        PropertyKey::string("with"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let obj = this_val.as_object().ok_or_else(|| {
                    VmError::type_error("Array.prototype.with: this is not an object")
                })?;
                let len = get_len(&obj) as i64;
                let raw_index = args.first().and_then(|v| v.as_number()).unwrap_or(0.0) as i64;
                let actual_index = if raw_index < 0 {
                    len + raw_index
                } else {
                    raw_index
                };
                if actual_index < 0 || actual_index >= len {
                    return Err(VmError::range_error(
                        "Array.prototype.with: index out of range",
                    ));
                }
                let value = args.get(1).cloned().unwrap_or(Value::undefined());
                let result =
                    GcRef::new(JsObject::array(len as usize, ncx.memory_manager().clone()));
                for i in 0..len {
                    if i & 0x3FF == 0 {
                        ncx.check_for_interrupt()?;
                    }
                    if i == actual_index {
                        let _ = result.set(PropertyKey::Index(i as u32), value.clone());
                    } else {
                        let val = js_get(&obj, &PropertyKey::Index(i as u32), ncx)?;
                        let _ = result.set(PropertyKey::Index(i as u32), val);
                    }
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

/// Install static methods on the Array constructor.
pub fn install_array_statics(
    ctor: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // Array.isArray(arg) — §23.1.2.2
    // Helper to recursively unwrap proxies when checking for arrays
    fn is_array_value(value: &Value) -> Result<bool, VmError> {
        if let Some(proxy) = value.as_proxy() {
            let target = proxy.target().ok_or_else(|| {
                VmError::type_error("Cannot perform 'isArray' on a proxy that has been revoked")
            })?;
            return is_array_value(&target);
        }
        if let Some(obj) = value.as_object() {
            return Ok(obj.is_array());
        }
        Ok(false)
    }

    let is_array_fn = Value::native_function_with_proto(
        |_this, args, _ncx| {
            let arg = args.first().cloned().unwrap_or(Value::undefined());
            let is_arr = is_array_value(&arg)?;
            Ok(Value::boolean(is_arr))
        },
        mm.clone(),
        fn_proto,
    );
    // Set function name and length, mark as non-constructor
    if let Some(obj) = is_array_fn.as_object() {
        // Array.isArray.length = 1
        obj.define_property(
            PropertyKey::string("length"),
            PropertyDescriptor::function_length(Value::int32(1)),
        );
        // Array.isArray.name = "isArray"
        obj.define_property(
            PropertyKey::string("name"),
            PropertyDescriptor::function_length(Value::string(JsString::intern("isArray"))),
        );
        // Mark as non-constructor so `new Array.isArray()` throws TypeError
        obj.define_property(
            PropertyKey::string("__non_constructor"),
            PropertyDescriptor::builtin_data(Value::boolean(true)),
        );
    }
    ctor.define_property(
        PropertyKey::string("isArray"),
        PropertyDescriptor::builtin_method(is_array_fn),
    );

    // Array.from(items [, mapFn [, thisArg]]) — §23.1.2.1
    ctor.define_property(
        PropertyKey::string("from"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |_this, args, ncx| {
                let source = args.first().cloned().unwrap_or(Value::undefined());
                let map_fn = args.get(1).cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(2).cloned().unwrap_or(Value::undefined());
                let has_map = !map_fn.is_undefined();
                if has_map && !map_fn.is_callable() {
                    return Err(VmError::type_error("Array.from: mapFn is not a function"));
                }

                // 1. Try iterator protocol via Symbol.iterator
                let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
                let iter_method = if let Some(obj) = source.as_object() {
                    obj.get(&PropertyKey::Symbol(iterator_sym))
                } else {
                    None
                };

                if let Some(iter_fn) = iter_method {
                    if iter_fn.is_callable() {
                        // Call [Symbol.iterator]() to get iterator
                        let iterator = ncx.call_function(&iter_fn, source.clone(), &[])?;
                        let result = GcRef::new(JsObject::array(0, ncx.memory_manager().clone()));
                        let mut k: u32 = 0;

                        loop {
                            if k & 0x3FF == 0 {
                                ncx.check_for_interrupt()?;
                            }
                            // Call iterator.next()
                            let iter_obj = iterator.as_object().ok_or_else(|| {
                                VmError::type_error("Array.from: iterator is not an object")
                            })?;
                            let next_fn =
                                iter_obj.get(&PropertyKey::string("next")).ok_or_else(|| {
                                    VmError::type_error("Array.from: iterator.next is not defined")
                                })?;
                            let next_result = ncx.call_function(&next_fn, iterator.clone(), &[])?;
                            let next_obj = next_result.as_object().ok_or_else(|| {
                                VmError::type_error("Array.from: iterator result is not an object")
                            })?;

                            // Check done
                            let done = next_obj
                                .get(&PropertyKey::string("done"))
                                .unwrap_or(Value::boolean(false));
                            if done.to_boolean() {
                                set_len(&result, k as usize);
                                return Ok(Value::array(result));
                            }

                            // Get value
                            let val = next_obj
                                .get(&PropertyKey::string("value"))
                                .unwrap_or(Value::undefined());

                            // Apply mapFn if provided
                            let mapped = if has_map {
                                ncx.call_function(
                                    &map_fn,
                                    this_arg.clone(),
                                    &[val, Value::number(k as f64)],
                                )?
                            } else {
                                val
                            };

                            let _ = result.set(PropertyKey::Index(k), mapped);
                            k += 1;
                        }
                    }
                }

                // 2. Array-like path (no iterator)
                if let Some(obj) = source.as_object() {
                    if let Some(len_val) = obj.get(&PropertyKey::string("length")) {
                        let len = len_val.as_number().unwrap_or(0.0).max(0.0) as usize;
                        let result = GcRef::new(JsObject::array(len, ncx.memory_manager().clone()));
                        for i in 0..len {
                            if i & 0x3FF == 0 {
                                ncx.check_for_interrupt()?;
                            }
                            let val = obj
                                .get(&PropertyKey::Index(i as u32))
                                .unwrap_or(Value::undefined());
                            let mapped = if has_map {
                                ncx.call_function(
                                    &map_fn,
                                    this_arg.clone(),
                                    &[val, Value::number(i as f64)],
                                )?
                            } else {
                                val
                            };
                            let _ = result.set(PropertyKey::Index(i as u32), mapped);
                        }
                        return Ok(Value::array(result));
                    }
                }

                // 3. String source — iterate code points
                if let Some(s) = source.as_string() {
                    let chars: Vec<char> = s.as_str().chars().collect();
                    let len = chars.len();
                    let result = GcRef::new(JsObject::array(len, ncx.memory_manager().clone()));
                    for (i, ch) in chars.iter().enumerate() {
                        let val = Value::string(JsString::intern(&ch.to_string()));
                        let mapped = if has_map {
                            ncx.call_function(
                                &map_fn,
                                this_arg.clone(),
                                &[val, Value::number(i as f64)],
                            )?
                        } else {
                            val
                        };
                        let _ = result.set(PropertyKey::Index(i as u32), mapped);
                    }
                    return Ok(Value::array(result));
                }

                Ok(Value::array(GcRef::new(JsObject::array(
                    0,
                    ncx.memory_manager().clone(),
                ))))
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
                    let _ = result.set(PropertyKey::Index(i as u32), arg.clone());
                }
                Ok(Value::array(result))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}
