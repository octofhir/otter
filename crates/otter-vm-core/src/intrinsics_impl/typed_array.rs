//! TypedArray prototypes and methods (ES2026 §22.2)
//!
//! Implements %TypedArray%.prototype and all 11 typed array prototypes:
//! - Int8Array, Uint8Array, Uint8ClampedArray
//! - Int16Array, Uint16Array
//! - Int32Array, Uint32Array
//! - Float32Array, Float64Array
//! - BigInt64Array, BigUint64Array
//!
//! ## Prototype Chain
//!
//! ```text
//! instance → Int8Array.prototype → %TypedArray%.prototype → Object.prototype → null
//! ```

use std::sync::Arc;

use crate::array_buffer::JsArrayBuffer;
use crate::error::VmError;
use crate::gc::GcRef;
use crate::memory::MemoryManager;
use crate::object::{JsObject, PropertyAttributes, PropertyDescriptor, PropertyKey};
use crate::string::JsString;
use crate::typed_array::{JsTypedArray, TypedArrayKind};
use crate::value::Value;

// ============================================================================
// %TypedArray%.prototype initialization
// ============================================================================

/// Initialize %TypedArray%.prototype with common methods shared by all typed arrays.
///
/// This implements ES2026 §22.2.3 - Properties of the %TypedArray% Prototype Object.
pub fn init_typed_array_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
    symbol_to_string_tag: crate::gc::GcRef<crate::value::Symbol>,
    array_iter_proto: GcRef<JsObject>,
) {
    // Getters (ES2026 §22.2.3.1-4)
    init_typed_array_getters(proto, fn_proto, mm);

    // Methods (ES2026 §22.2.3.5-32)
    init_typed_array_methods(proto, fn_proto, mm);

    // Iterators (ES2026 §22.2.3.6, 11, 29, 31)
    init_typed_array_iterators(proto, fn_proto, mm, symbol_iterator, array_iter_proto);

    // %TypedArray%.prototype[Symbol.toStringTag] = "TypedArray"
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("TypedArray")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

/// Initialize individual typed array prototype (Int8Array.prototype, etc.)
///
/// Each specific prototype gets its own Symbol.toStringTag.
pub fn init_specific_typed_array_prototype(
    proto: GcRef<JsObject>,
    kind: TypedArrayKind,
    symbol_to_string_tag: crate::gc::GcRef<crate::value::Symbol>,
) {
    // Int8Array.prototype[Symbol.toStringTag] = "Int8Array"
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern(kind.name())),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

// ============================================================================
// Getters
// ============================================================================

/// Helper to get TypedArray from this_val (handles both direct value and hidden property)
fn get_typed_array(this_val: &Value) -> Result<GcRef<JsTypedArray>, VmError> {
    // Try to get TypedArray from the value directly first
    if let Some(ta) = this_val.as_typed_array() {
        return Ok(ta);
    }

    // If this_val is an object, try to get the TypedArray from a hidden property
    if let Some(obj) = this_val.as_object() {
        if let Some(ta_val) = obj.get(&PropertyKey::string("__TypedArrayData__")) {
            if let Some(ta) = ta_val.as_typed_array() {
                return Ok(ta);
            }
        }
    }

    Err(VmError::type_error("Method called on non-TypedArray"))
}

fn init_typed_array_getters(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // get %TypedArray%.prototype.buffer (ES2026 §22.2.3.1)
    proto.define_property(
        PropertyKey::string("buffer"),
        PropertyDescriptor::getter(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ta = get_typed_array(this_val)?;
                Ok(Value::array_buffer(ta.buffer().clone()))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // get %TypedArray%.prototype.byteLength (ES2026 §22.2.3.2)
    proto.define_property(
        PropertyKey::string("byteLength"),
        PropertyDescriptor::getter(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ta = get_typed_array(this_val)?;
                Ok(Value::int32(ta.byte_length() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // get %TypedArray%.prototype.byteOffset (ES2026 §22.2.3.3)
    proto.define_property(
        PropertyKey::string("byteOffset"),
        PropertyDescriptor::getter(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ta = get_typed_array(this_val)?;
                Ok(Value::int32(ta.byte_offset() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // get %TypedArray%.prototype.length (ES2026 §22.2.3.4)
    proto.define_property(
        PropertyKey::string("length"),
        PropertyDescriptor::getter(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ta = get_typed_array(this_val)?;
                Ok(Value::int32(ta.length() as i32))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

// ============================================================================
// Methods
// ============================================================================

fn init_typed_array_methods(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
) {
    // %TypedArray%.prototype.at(index) — ES2022 §22.2.3.5
    proto.define_property(
        PropertyKey::string("at"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let index = args.first().and_then(|v| v.as_int32()).unwrap_or(0);

                let length = ta.length() as i32;
                let actual_index = if index < 0 {
                    (length + index) as usize
                } else {
                    index as usize
                };

                if actual_index >= ta.length() {
                    return Ok(Value::undefined());
                }

                match ta.get(actual_index) {
                    Some(val) => Ok(Value::number(val)),
                    None => Ok(Value::undefined()),
                }
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.copyWithin(target, start, end) — ES2026 §22.2.3.8
    proto.define_property(
        PropertyKey::string("copyWithin"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let target = args.get(0).and_then(|v| v.as_int32()).unwrap_or(0) as i64;
                let start = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as i64;
                let end = args.get(2).and_then(|v| v.as_int32()).map(|v| v as i64);

                ta.copy_within(target, start, end);

                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.fill(value, start, end) — ES2026 §22.2.3.11
    proto.define_property(
        PropertyKey::string("fill"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let value = args.first().and_then(|v| v.as_number()).unwrap_or(0.0);

                let start = args.get(1).and_then(|v| v.as_int32()).map(|v| v as i64);

                let end = args.get(2).and_then(|v| v.as_int32()).map(|v| v as i64);

                ta.fill(value, start, end);

                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.includes(searchElement, fromIndex) — ES2026 §22.2.3.13
    proto.define_property(
        PropertyKey::string("includes"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let search = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);

                let from_index = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as usize;

                for i in from_index..ta.length() {
                    if let Some(val) = ta.get(i) {
                        if (val.is_nan() && search.is_nan()) || val == search {
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

    // %TypedArray%.prototype.indexOf(searchElement, fromIndex) — ES2026 §22.2.3.14
    proto.define_property(
        PropertyKey::string("indexOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let search = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);

                let from_index = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as usize;

                for i in from_index..ta.length() {
                    if let Some(val) = ta.get(i) {
                        if val == search {
                            return Ok(Value::int32(i as i32));
                        }
                    }
                }

                Ok(Value::int32(-1))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.join(separator) — ES2026 §22.2.3.15
    proto.define_property(
        PropertyKey::string("join"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let separator = args
                    .first()
                    .and_then(|v| v.as_string())
                    .map(|s| s.as_str().to_owned())
                    .unwrap_or_else(|| ",".to_owned());

                let mut result = String::new();
                for i in 0..ta.length() {
                    if i > 0 {
                        result.push_str(&separator);
                    }
                    if let Some(val) = ta.get(i) {
                        result.push_str(&val.to_string());
                    }
                }

                Ok(Value::string(JsString::intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.lastIndexOf(searchElement, fromIndex) — ES2026 §22.2.3.16
    proto.define_property(
        PropertyKey::string("lastIndexOf"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let search = args.first().and_then(|v| v.as_number()).unwrap_or(f64::NAN);

                let from_index = args
                    .get(1)
                    .and_then(|v| v.as_int32())
                    .map(|v| v as usize)
                    .unwrap_or(ta.length().saturating_sub(1));

                for i in (0..=from_index.min(ta.length().saturating_sub(1))).rev() {
                    if let Some(val) = ta.get(i) {
                        if val == search {
                            return Ok(Value::int32(i as i32));
                        }
                    }
                }

                Ok(Value::int32(-1))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.reverse() — ES2026 §22.2.3.22
    proto.define_property(
        PropertyKey::string("reverse"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ta = get_typed_array(this_val)?;

                ta.reverse();

                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.set(source, offset) — ES2026 §22.2.3.23
    proto.define_property(
        PropertyKey::string("set"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let offset = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as usize;

                if let Some(source) = args.first() {
                    // Check if source is a TypedArray
                    if let Some(source_ta) = source.as_typed_array() {
                        for i in 0..source_ta.length() {
                            if let Some(val) = source_ta.get(i) {
                                let _ = ta.set(offset + i, val);
                            }
                        }
                    } else if let Some(source_obj) = source.as_object() {
                        // Array-like object
                        if let Some(length_val) = source_obj.get(&PropertyKey::string("length")) {
                            if let Some(length) = length_val.as_int32() {
                                for i in 0..(length as usize) {
                                    if let Some(val) = source_obj.get(&PropertyKey::Index(i as u32))
                                    {
                                        if let Some(num) = val.as_number() {
                                            let _ = ta.set(offset + i, num);
                                        }
                                    }
                                }
                            }
                        }
                    }
                }

                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.slice(start, end) — ES2026 §22.2.3.24
    proto.define_property(
        PropertyKey::string("slice"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let start = args.get(0).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

                let end = args.get(1).and_then(|v| v.as_int32()).map(|v| v as i64);

                let new_ta = ta.slice(start, end).map_err(|e| VmError::type_error(e))?;

                Ok(Value::typed_array(GcRef::new(new_ta)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.subarray(begin, end) — ES2026 §22.2.3.27
    proto.define_property(
        PropertyKey::string("subarray"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let begin = args.get(0).and_then(|v| v.as_int32()).unwrap_or(0) as i64;

                let end = args.get(1).and_then(|v| v.as_int32()).map(|v| v as i64);

                let new_ta = ta
                    .subarray(begin, end)
                    .map_err(|e| VmError::type_error(e))?;

                Ok(Value::typed_array(GcRef::new(new_ta)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.toString() — ES2026 §22.2.3.29
    proto.define_property(
        PropertyKey::string("toString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                // Delegate to Array.prototype.toString which calls join()
                let ta = get_typed_array(this_val)?;

                let mut result = String::new();
                for i in 0..ta.length() {
                    if i > 0 {
                        result.push(',');
                    }
                    if let Some(val) = ta.get(i) {
                        result.push_str(&val.to_string());
                    }
                }

                Ok(Value::string(JsString::intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.toLocaleString() — ES2026 §22.2.3.28
    // For now, just delegate to toString
    proto.define_property(
        PropertyKey::string("toLocaleString"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, _args, _ncx| {
                let ta = get_typed_array(this_val)?;

                let mut result = String::new();
                for i in 0..ta.length() {
                    if i > 0 {
                        result.push(',');
                    }
                    if let Some(val) = ta.get(i) {
                        result.push_str(&val.to_string());
                    }
                }

                Ok(Value::string(JsString::intern(&result)))
            },
            mm.clone(),
            fn_proto,
        )),
    );
}

// ============================================================================
// Iterators
// ============================================================================

fn init_typed_array_iterators(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_iterator: crate::gc::GcRef<crate::value::Symbol>,
    array_iter_proto: GcRef<JsObject>,
) {
    // %TypedArray%.prototype.values() — uses shared %ArrayIteratorPrototype%
    let values_method = Value::native_function_with_proto(
        move |this_val, _args, ncx| {
            let ta = get_typed_array(this_val)?;
            let ta_val = Value::typed_array(ta);
            super::array::make_array_iterator(&ta_val, "value", ncx)
        },
        mm.clone(),
        fn_proto,
    );

    proto.define_property(
        PropertyKey::string("values"),
        PropertyDescriptor::builtin_method(values_method.clone()),
    );

    // %TypedArray%.prototype[Symbol.iterator] = %TypedArray%.prototype.values
    proto.define_property(
        PropertyKey::Symbol(symbol_iterator),
        PropertyDescriptor::builtin_method(values_method),
    );

    // %TypedArray%.prototype.keys()
    let _ = array_iter_proto;
    proto.define_property(
        PropertyKey::string("keys"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                let ta = get_typed_array(this_val)?;
                let ta_val = Value::typed_array(ta);
                super::array::make_array_iterator(&ta_val, "key", ncx)
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.entries()
    proto.define_property(
        PropertyKey::string("entries"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            move |this_val, _args, ncx| {
                let ta = get_typed_array(this_val)?;
                let ta_val = Value::typed_array(ta);
                super::array::make_array_iterator(&ta_val, "entry", ncx)
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // ========================================================================
    // Callback-based iteration methods
    // ========================================================================

    // Helper: create a new typed array of a given kind and length
    fn create_typed_array(
        kind: TypedArrayKind,
        length: usize,
        proto: Value,
        mm: Arc<MemoryManager>,
    ) -> Result<JsTypedArray, VmError> {
        let byte_len = length
            .checked_mul(kind.element_size())
            .ok_or_else(|| VmError::range_error("Invalid typed array length"))?;
        let buffer = GcRef::new(JsArrayBuffer::new(byte_len, None, mm.clone()));
        let object = GcRef::new(JsObject::new(proto, mm));
        JsTypedArray::new(object, buffer, kind, 0, length).map_err(|e| VmError::type_error(e))
    }

    // Helper: get element at index as Value (handles BigInt arrays)
    fn ta_element_value(ta: &JsTypedArray, index: usize) -> Value {
        if ta.kind().is_bigint() {
            if let Some(v) = ta.get_bigint(index) {
                Value::bigint(v.to_string())
            } else {
                Value::undefined()
            }
        } else if let Some(v) = ta.get(index) {
            Value::number(v)
        } else {
            Value::undefined()
        }
    }

    // Helper: extract a numeric value from a callback result and write to typed array
    fn ta_set_from_value(ta: &JsTypedArray, index: usize, val: &Value) {
        if ta.kind().is_bigint() {
            // Try to get BigInt value
            if let Some(crate::value::HeapRef::BigInt(b)) = val.heap_ref() {
                if let Ok(n) = b.value.parse::<i64>() {
                    ta.set_bigint(index, n);
                }
            } else {
                let n = crate::globals::to_number(val) as i64;
                ta.set_bigint(index, n);
            }
        } else {
            let n = crate::globals::to_number(val);
            ta.set(index, n);
        }
    }

    // %TypedArray%.prototype.every(callbackfn [, thisArg]) — §22.2.3.8
    proto.define_property(
        PropertyKey::string("every"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                for i in 0..len {
                    let val = ta_element_value(&ta, i);
                    let result = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
                    if !result.to_boolean() {
                        return Ok(Value::boolean(false));
                    }
                }
                Ok(Value::boolean(true))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.some(callbackfn [, thisArg]) — §22.2.3.29
    proto.define_property(
        PropertyKey::string("some"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                for i in 0..len {
                    let val = ta_element_value(&ta, i);
                    let result = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
                    if result.to_boolean() {
                        return Ok(Value::boolean(true));
                    }
                }
                Ok(Value::boolean(false))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.forEach(callbackfn [, thisArg]) — §22.2.3.13
    proto.define_property(
        PropertyKey::string("forEach"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                for i in 0..len {
                    let val = ta_element_value(&ta, i);
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

    // %TypedArray%.prototype.find(predicate [, thisArg]) — §22.2.3.12
    proto.define_property(
        PropertyKey::string("find"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                for i in 0..len {
                    let val = ta_element_value(&ta, i);
                    let result = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val.clone(), Value::number(i as f64), this_val.clone()],
                    )?;
                    if result.to_boolean() {
                        return Ok(val);
                    }
                }
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.findIndex(predicate [, thisArg]) — §22.2.3.11
    proto.define_property(
        PropertyKey::string("findIndex"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                for i in 0..len {
                    let val = ta_element_value(&ta, i);
                    let result = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
                    if result.to_boolean() {
                        return Ok(Value::number(i as f64));
                    }
                }
                Ok(Value::int32(-1))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.findLast(predicate [, thisArg]) — §22.2.3.11.1
    proto.define_property(
        PropertyKey::string("findLast"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                for i in (0..len).rev() {
                    let val = ta_element_value(&ta, i);
                    let result = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val.clone(), Value::number(i as f64), this_val.clone()],
                    )?;
                    if result.to_boolean() {
                        return Ok(val);
                    }
                }
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.findLastIndex(predicate [, thisArg])
    proto.define_property(
        PropertyKey::string("findLastIndex"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                for i in (0..len).rev() {
                    let val = ta_element_value(&ta, i);
                    let result = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val, Value::number(i as f64), this_val.clone()],
                    )?;
                    if result.to_boolean() {
                        return Ok(Value::number(i as f64));
                    }
                }
                Ok(Value::int32(-1))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.map(callbackfn [, thisArg]) — §22.2.3.20
    proto.define_property(
        PropertyKey::string("map"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                let kind = ta.kind();
                let src_proto = this_val
                    .as_object()
                    .map(|o| o.prototype().clone())
                    .unwrap_or(Value::null());
                let new_ta =
                    create_typed_array(kind, len, src_proto, ncx.memory_manager().clone())?;
                // Root the new TypedArray's object across call_function GC points
                ncx.ctx.push_root_slot(Value::object(new_ta.object));
                let loop_result: Result<(), VmError> = (|| {
                    for i in 0..len {
                        let val = ta_element_value(&ta, i);
                        let mapped = ncx.call_function(
                            &callback,
                            this_arg.clone(),
                            &[val, Value::number(i as f64), this_val.clone()],
                        )?;
                        ta_set_from_value(&new_ta, i, &mapped);
                    }
                    Ok(())
                })();
                ncx.ctx.pop_root_slots(1);
                loop_result?;
                let obj = new_ta.object;
                obj.define_property(
                    PropertyKey::string("__TypedArrayData__"),
                    PropertyDescriptor::data(Value::typed_array(GcRef::new(new_ta))),
                );
                Ok(Value::object(obj))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.filter(callbackfn [, thisArg]) — §22.2.3.10
    proto.define_property(
        PropertyKey::string("filter"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let this_arg = args.get(1).cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                let kind = ta.kind();
                let mut kept: Vec<Value> = Vec::new();
                for i in 0..len {
                    let val = ta_element_value(&ta, i);
                    let result = ncx.call_function(
                        &callback,
                        this_arg.clone(),
                        &[val.clone(), Value::number(i as f64), this_val.clone()],
                    )?;
                    if result.to_boolean() {
                        kept.push(val);
                    }
                }
                let src_proto = this_val
                    .as_object()
                    .map(|o| o.prototype().clone())
                    .unwrap_or(Value::null());
                let new_ta =
                    create_typed_array(kind, kept.len(), src_proto, ncx.memory_manager().clone())?;
                for (i, val) in kept.iter().enumerate() {
                    ta_set_from_value(&new_ta, i, val);
                }
                let obj = new_ta.object;
                obj.define_property(
                    PropertyKey::string("__TypedArrayData__"),
                    PropertyDescriptor::data(Value::typed_array(GcRef::new(new_ta))),
                );
                Ok(Value::object(obj))
            },
            mm.clone(),
            fn_proto,
        )),
    );

    // %TypedArray%.prototype.reduce(callbackfn [, initialValue]) — §22.2.3.22
    proto.define_property(
        PropertyKey::string("reduce"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                let mut accumulator;
                let start;
                if args.len() >= 2 {
                    accumulator = args[1].clone();
                    start = 0;
                } else {
                    if len == 0 {
                        return Err(VmError::type_error(
                            "Reduce of empty array with no initial value",
                        ));
                    }
                    accumulator = ta_element_value(&ta, 0);
                    start = 1;
                }
                for i in start..len {
                    let val = ta_element_value(&ta, i);
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

    // %TypedArray%.prototype.reduceRight(callbackfn [, initialValue]) — §22.2.3.23
    proto.define_property(
        PropertyKey::string("reduceRight"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let callback = args.first().cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                let mut accumulator;
                let start;
                if args.len() >= 2 {
                    accumulator = args[1].clone();
                    start = len;
                } else {
                    if len == 0 {
                        return Err(VmError::type_error(
                            "Reduce of empty array with no initial value",
                        ));
                    }
                    accumulator = ta_element_value(&ta, len - 1);
                    start = len - 1;
                }
                for i in (0..start).rev() {
                    let val = ta_element_value(&ta, i);
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

    // %TypedArray%.prototype.sort([comparefn]) — §22.2.3.30
    proto.define_property(
        PropertyKey::string("sort"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, ncx| {
                let ta = get_typed_array(this_val)?;
                let compare_fn = args.first().cloned().unwrap_or(Value::undefined());
                let len = ta.length();
                if len <= 1 {
                    return Ok(this_val.clone());
                }

                // Collect values
                let mut values: Vec<f64> = (0..len).filter_map(|i| ta.get(i)).collect();

                if compare_fn.is_undefined() {
                    // Default numeric sort
                    values.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
                } else {
                    // Custom comparator — use a simple insertion sort to avoid
                    // issues with sort_by requiring Fn not FnMut
                    for i in 1..values.len() {
                        let key = values[i];
                        let mut j = i;
                        while j > 0 {
                            let cmp_result = ncx.call_function(
                                &compare_fn,
                                Value::undefined(),
                                &[Value::number(values[j - 1]), Value::number(key)],
                            )?;
                            let cmp_val = crate::globals::to_number(&cmp_result);
                            if cmp_val > 0.0 {
                                values[j] = values[j - 1];
                                j -= 1;
                            } else {
                                break;
                            }
                        }
                        values[j] = key;
                    }
                }

                // Write back
                for (i, &v) in values.iter().enumerate() {
                    ta.set(i, v);
                }
                Ok(this_val.clone())
            },
            mm.clone(),
            fn_proto,
        )),
    );
}
