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
use crate::builtin_builder::{BuiltInBuilder, IntrinsicContext, IntrinsicObject};
use crate::data_view::JsDataView;
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

pub fn init_array_buffer_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_to_string_tag: crate::gc::GcRef<crate::value::Symbol>,
) {
    proto.define_property(
        PropertyKey::string("byteLength"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _, _ncx| {
                    if let Some(ab) = this_val.as_array_buffer() {
                        Ok(Value::number(ab.byte_length() as f64))
                    } else {
                        Err(VmError::type_error("not an ArrayBuffer"))
                    }
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: PropertyAttributes::builtin_method(),
        },
    );
    proto.define_property(
        PropertyKey::string("slice"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let ab = this_val
                    .as_array_buffer()
                    .ok_or_else(|| VmError::type_error("not an ArrayBuffer"))?;
                let len = ab.byte_length();
                let start = args
                    .first()
                    .map(|v| {
                        let n = crate::globals::to_number(v) as isize;
                        if n < 0 {
                            (len as isize + n).max(0) as usize
                        } else {
                            n.min(len as isize) as usize
                        }
                    })
                    .unwrap_or(0);
                let end = args
                    .get(1)
                    .map(|v| {
                        let n = crate::globals::to_number(v) as isize;
                        if n < 0 {
                            (len as isize + n).max(0) as usize
                        } else {
                            n.min(len as isize) as usize
                        }
                    })
                    .unwrap_or(len);
                let new_len = if end > start { end - start } else { 0 };
                let new_ab = GcRef::new(JsArrayBuffer::new(new_len, None));
                if new_len > 0 {
                    ab.with_data(|src| {
                        new_ab.with_data_mut(|dst| {
                            dst[..new_len].copy_from_slice(&src[start..start + new_len]);
                        });
                    });
                }
                Ok(Value::array_buffer(new_ab))
            },
            mm.clone(),
            fn_proto,
        )),
    );
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("ArrayBuffer")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

pub fn init_data_view_prototype(
    proto: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    mm: &Arc<MemoryManager>,
    symbol_to_string_tag: crate::gc::GcRef<crate::value::Symbol>,
) {
    macro_rules! dv_getter {
        ($name:expr, $method:ident, $size:ty) => {
            proto.define_property(
                PropertyKey::string($name),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    |this_val, args, _ncx| {
                        let dv = this_val
                            .as_data_view()
                            .ok_or_else(|| VmError::type_error("not a DataView"))?;
                        let offset = args
                            .first()
                            .map(|v| crate::globals::to_number(v) as usize)
                            .unwrap_or(0);
                        let little_endian = args.get(1).map(|v| v.to_boolean()).unwrap_or(false);
                        let val = dv
                            .$method(offset, little_endian)
                            .map_err(VmError::type_error)?;
                        Ok(Value::number(val as f64))
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );
        };
        ($name:expr, $method:ident) => {
            proto.define_property(
                PropertyKey::string($name),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    |this_val, args, _ncx| {
                        let dv = this_val
                            .as_data_view()
                            .ok_or_else(|| VmError::type_error("not a DataView"))?;
                        let offset = args
                            .first()
                            .map(|v| crate::globals::to_number(v) as usize)
                            .unwrap_or(0);
                        let val = dv.$method(offset).map_err(VmError::type_error)?;
                        Ok(Value::number(val as f64))
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );
        };
    }

    macro_rules! dv_setter {
        ($name:expr, $method:ident, $conv:expr) => {
            proto.define_property(
                PropertyKey::string($name),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    |this_val, args, _ncx| {
                        let dv = this_val
                            .as_data_view()
                            .ok_or_else(|| VmError::type_error("not a DataView"))?;
                        let offset = args
                            .first()
                            .map(|v| crate::globals::to_number(v) as usize)
                            .unwrap_or(0);
                        let raw = args.get(1).map(crate::globals::to_number).unwrap_or(0.0);
                        let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);
                        let val = ($conv)(raw);
                        dv.$method(offset, val, little_endian)
                            .map_err(VmError::type_error)?;
                        Ok(Value::undefined())
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );
        };
        ($name:expr, $method:ident, $conv:expr, no_endian) => {
            proto.define_property(
                PropertyKey::string($name),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    |this_val, args, _ncx| {
                        let dv = this_val
                            .as_data_view()
                            .ok_or_else(|| VmError::type_error("not a DataView"))?;
                        let offset = args
                            .first()
                            .map(|v| crate::globals::to_number(v) as usize)
                            .unwrap_or(0);
                        let raw = args.get(1).map(crate::globals::to_number).unwrap_or(0.0);
                        let val = ($conv)(raw);
                        dv.$method(offset, val).map_err(VmError::type_error)?;
                        Ok(Value::undefined())
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );
        };
    }

    dv_getter!("getInt8", get_int8);
    dv_getter!("getUint8", get_uint8);
    dv_getter!("getInt16", get_int16, i16);
    dv_getter!("getUint16", get_uint16, u16);
    dv_getter!("getInt32", get_int32, i32);
    dv_getter!("getUint32", get_uint32, u32);
    dv_getter!("getFloat32", get_float32, f32);
    dv_getter!("getFloat64", get_float64, f64);

    dv_setter!("setInt8", set_int8, |v: f64| (v as i32) as i8, no_endian);
    dv_setter!("setUint8", set_uint8, |v: f64| (v as u32) as u8, no_endian);
    dv_setter!("setInt16", set_int16, |v: f64| (v as i32) as i16);
    dv_setter!("setUint16", set_uint16, |v: f64| (v as u32) as u16);
    dv_setter!("setInt32", set_int32, |v: f64| v as i32);
    dv_setter!("setUint32", set_uint32, |v: f64| v as u32);
    dv_setter!("setFloat32", set_float32, |v: f64| v as f32);
    dv_setter!("setFloat64", set_float64, |v: f64| v as f64);

    proto.define_property(
        PropertyKey::string("getBigInt64"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let dv = this_val
                    .as_data_view()
                    .ok_or_else(|| VmError::type_error("not a DataView"))?;
                let offset = args
                    .first()
                    .map(|v| crate::globals::to_number(v) as usize)
                    .unwrap_or(0);
                let little_endian = args.get(1).map(|v| v.to_boolean()).unwrap_or(false);
                let val = dv
                    .get_big_int64(offset, little_endian)
                    .map_err(VmError::type_error)?;
                Ok(Value::bigint(val.to_string()))
            },
            mm.clone(),
            fn_proto,
        )),
    );
    proto.define_property(
        PropertyKey::string("getBigUint64"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let dv = this_val
                    .as_data_view()
                    .ok_or_else(|| VmError::type_error("not a DataView"))?;
                let offset = args
                    .first()
                    .map(|v| crate::globals::to_number(v) as usize)
                    .unwrap_or(0);
                let little_endian = args.get(1).map(|v| v.to_boolean()).unwrap_or(false);
                let val = dv
                    .get_big_uint64(offset, little_endian)
                    .map_err(VmError::type_error)?;
                Ok(Value::bigint(val.to_string()))
            },
            mm.clone(),
            fn_proto,
        )),
    );
    proto.define_property(
        PropertyKey::string("setBigInt64"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let dv = this_val
                    .as_data_view()
                    .ok_or_else(|| VmError::type_error("not a DataView"))?;
                let offset = args
                    .first()
                    .map(|v| crate::globals::to_number(v) as usize)
                    .unwrap_or(0);
                let val = args
                    .get(1)
                    .map(|v| crate::globals::to_number(v) as i64)
                    .unwrap_or(0);
                let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);
                dv.set_big_int64(offset, val, little_endian)
                    .map_err(VmError::type_error)?;
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );
    proto.define_property(
        PropertyKey::string("setBigUint64"),
        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
            |this_val, args, _ncx| {
                let dv = this_val
                    .as_data_view()
                    .ok_or_else(|| VmError::type_error("not a DataView"))?;
                let offset = args
                    .first()
                    .map(|v| crate::globals::to_number(v) as usize)
                    .unwrap_or(0);
                let val = args
                    .get(1)
                    .map(|v| crate::globals::to_number(v) as u64)
                    .unwrap_or(0);
                let little_endian = args.get(2).map(|v| v.to_boolean()).unwrap_or(false);
                dv.set_big_uint64(offset, val, little_endian)
                    .map_err(VmError::type_error)?;
                Ok(Value::undefined())
            },
            mm.clone(),
            fn_proto,
        )),
    );

    proto.define_property(
        PropertyKey::string("buffer"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _, _ncx| {
                    let dv = this_val
                        .as_data_view()
                        .ok_or_else(|| VmError::type_error("not a DataView"))?;
                    Ok(Value::array_buffer(dv.buffer()))
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: PropertyAttributes::builtin_method(),
        },
    );
    proto.define_property(
        PropertyKey::string("byteLength"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _, _ncx| {
                    let dv = this_val
                        .as_data_view()
                        .ok_or_else(|| VmError::type_error("not a DataView"))?;
                    Ok(Value::number(dv.byte_length() as f64))
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: PropertyAttributes::builtin_method(),
        },
    );
    proto.define_property(
        PropertyKey::string("byteOffset"),
        PropertyDescriptor::Accessor {
            get: Some(Value::native_function_with_proto(
                |this_val, _, _ncx| {
                    let dv = this_val
                        .as_data_view()
                        .ok_or_else(|| VmError::type_error("not a DataView"))?;
                    Ok(Value::number(dv.byte_offset() as f64))
                },
                mm.clone(),
                fn_proto,
            )),
            set: None,
            attributes: PropertyAttributes::builtin_method(),
        },
    );
    proto.define_property(
        PropertyKey::Symbol(symbol_to_string_tag),
        PropertyDescriptor::data_with_attrs(
            Value::string(JsString::intern("DataView")),
            PropertyAttributes {
                writable: false,
                enumerable: false,
                configurable: true,
            },
        ),
    );
}

fn make_typed_array_object(ta: JsTypedArray) -> Value {
    let ta_arc = GcRef::new(ta);
    let obj = ta_arc.object;
    obj.define_property(
        PropertyKey::string("__TypedArrayData__"),
        PropertyDescriptor::data(Value::typed_array(ta_arc)),
    );
    Value::object(obj)
}

fn create_typed_array_constructor(
    kind: TypedArrayKind,
    proto: GcRef<JsObject>,
) -> impl Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
+ Send
+ Sync
+ 'static {
    move |_this, args, ncx| {
        if args.is_empty() {
            let buffer = GcRef::new(JsArrayBuffer::new(0, None));
            let object = GcRef::new(JsObject::new(Value::object(proto)));
            let ta = JsTypedArray::new(object, buffer, kind, 0, 0).map_err(VmError::type_error)?;
            return Ok(make_typed_array_object(ta));
        }

        let arg0 = &args[0];

        if let Some(buffer) = arg0.as_array_buffer() {
            let byte_offset = args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as usize;
            let length = if let Some(len_val) = args.get(2) {
                len_val.as_int32().unwrap_or(0) as usize
            } else {
                let available = buffer.byte_length().saturating_sub(byte_offset);
                available / kind.element_size()
            };

            let object = GcRef::new(JsObject::new(Value::object(proto)));
            let ta = JsTypedArray::new(object, buffer.clone(), kind, byte_offset, length)
                .map_err(VmError::type_error)?;
            return Ok(make_typed_array_object(ta));
        }

        if let Some(other_ta) = arg0.as_typed_array() {
            let length = other_ta.length();
            let byte_len = length
                .checked_mul(kind.element_size())
                .ok_or_else(|| VmError::range_error("Invalid typed array length"))?;
            let buffer = GcRef::new(JsArrayBuffer::new(byte_len, None));
            let object = GcRef::new(JsObject::new(Value::object(proto)));
            let ta =
                JsTypedArray::new(object, buffer, kind, 0, length).map_err(VmError::type_error)?;
            for i in 0..length {
                if let Some(val) = other_ta.get(i) {
                    let _ = ta.set(i, val);
                }
            }
            return Ok(make_typed_array_object(ta));
        }

        if let Some(length_num) = arg0.as_number() {
            let length = if length_num < 0.0 || length_num.is_nan() {
                return Err(VmError::range_error("Invalid typed array length"));
            } else if length_num > (usize::MAX / 8) as f64 {
                return Err(VmError::range_error("Invalid typed array length"));
            } else {
                length_num as usize
            };
            let byte_len = length
                .checked_mul(kind.element_size())
                .ok_or_else(|| VmError::range_error("Invalid typed array length"))?;
            let buffer = GcRef::new(JsArrayBuffer::new(byte_len, None));
            let object = GcRef::new(JsObject::new(Value::object(proto)));
            let ta =
                JsTypedArray::new(object, buffer, kind, 0, length).map_err(VmError::type_error)?;
            return Ok(make_typed_array_object(ta));
        }

        if let Some(length_int) = arg0.as_int32() {
            if length_int < 0 {
                return Err(VmError::range_error("Invalid typed array length"));
            }
            let length = length_int as usize;
            let byte_len = length
                .checked_mul(kind.element_size())
                .ok_or_else(|| VmError::range_error("Invalid typed array length"))?;
            let buffer = GcRef::new(JsArrayBuffer::new(byte_len, None));
            let object = GcRef::new(JsObject::new(Value::object(proto)));
            let ta =
                JsTypedArray::new(object, buffer, kind, 0, length).map_err(VmError::type_error)?;
            return Ok(make_typed_array_object(ta));
        }

        if let Some(obj) = arg0.as_object() {
            let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
            if let Some(iter_fn) = obj.get(&PropertyKey::Symbol(iterator_sym))
                && iter_fn.is_callable()
            {
                let iterator = ncx.call_function(&iter_fn, arg0.clone(), &[])?;
                let iter_obj = iterator.as_object().ok_or_else(|| {
                    VmError::type_error("TypedArray constructor: iterator is not an object")
                })?;
                let mut values: Vec<Value> = Vec::new();
                loop {
                    let next_fn = iter_obj.get(&PropertyKey::string("next")).ok_or_else(|| {
                        VmError::type_error("TypedArray constructor: iterator.next is not defined")
                    })?;
                    if !next_fn.is_callable() {
                        return Err(VmError::type_error(
                            "TypedArray constructor: iterator.next is not callable",
                        ));
                    }
                    let next_result = ncx.call_function(&next_fn, iterator.clone(), &[])?;
                    let next_obj = next_result.as_object().ok_or_else(|| {
                        VmError::type_error(
                            "TypedArray constructor: iterator result is not an object",
                        )
                    })?;
                    let done = next_obj
                        .get(&PropertyKey::string("done"))
                        .unwrap_or(Value::boolean(false))
                        .to_boolean();
                    if done {
                        break;
                    }
                    values.push(
                        next_obj
                            .get(&PropertyKey::string("value"))
                            .unwrap_or(Value::undefined()),
                    );
                }

                let length = values.len();
                let byte_len = length
                    .checked_mul(kind.element_size())
                    .ok_or_else(|| VmError::range_error("Invalid typed array length"))?;
                let buffer = GcRef::new(JsArrayBuffer::new(byte_len, None));
                let object = GcRef::new(JsObject::new(Value::object(proto)));
                let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                    .map_err(VmError::type_error)?;

                for (i, val) in values.into_iter().enumerate() {
                    if kind.is_bigint() {
                        if let Some(b) = val.as_bigint()
                            && let Ok(bigint) = b.value.parse::<i64>()
                        {
                            let _ = ta.set_bigint(i, bigint);
                        }
                    } else if let Some(num) = val.as_number() {
                        let _ = ta.set(i, num);
                    } else if let Some(num_int) = val.as_int32() {
                        let _ = ta.set(i, num_int as f64);
                    }
                }

                return Ok(make_typed_array_object(ta));
            }

            if let Some(length_val) = obj.get(&PropertyKey::string("length")) {
                if let Some(length) = length_val.as_int32() {
                    let length = length.max(0) as usize;
                    let buffer = GcRef::new(JsArrayBuffer::new(length * kind.element_size(), None));
                    let object = GcRef::new(JsObject::new(Value::object(proto)));
                    let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                        .map_err(VmError::type_error)?;

                    for i in 0..length {
                        if let Some(val) = obj.get(&PropertyKey::Index(i as u32)) {
                            if let Some(num) = val.as_number() {
                                let _ = ta.set(i, num);
                            } else if let Some(num_int) = val.as_int32() {
                                let _ = ta.set(i, num_int as f64);
                            }
                        }
                    }

                    return Ok(make_typed_array_object(ta));
                }
            }
        }

        let buffer = GcRef::new(JsArrayBuffer::new(0, None));
        let object = GcRef::new(JsObject::new(Value::object(proto)));
        let ta = JsTypedArray::new(object, buffer, kind, 0, 0).map_err(VmError::type_error)?;
        Ok(make_typed_array_object(ta))
    }
}

fn typed_array_from_static(
    this_val: &Value,
    args: &[Value],
    ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let source = args
        .first()
        .ok_or_else(|| VmError::type_error("TypedArray.from requires a source argument"))?;
    let map_fn = args.get(1).cloned().unwrap_or(Value::undefined());
    let this_arg = args.get(2).cloned().unwrap_or(Value::undefined());
    let has_map = !map_fn.is_undefined();
    if has_map && !map_fn.is_callable() {
        return Err(VmError::type_error(
            "TypedArray.from: mapFn is not a function",
        ));
    }

    let this_obj = this_val
        .as_object()
        .ok_or_else(|| VmError::type_error("TypedArray.from: this is not a constructor"))?;
    if !this_val.is_callable()
        || this_obj
            .get(&PropertyKey::string("prototype"))
            .and_then(|v| v.as_object())
            .is_none()
    {
        return Err(VmError::type_error(
            "TypedArray.from: this is not a constructor",
        ));
    }

    let mut values: Vec<Value> = Vec::new();
    let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
    let using_iterator = if let Some(obj) = source.as_object() {
        crate::object::get_value_full(&obj, &PropertyKey::Symbol(iterator_sym), ncx)?
    } else {
        Value::undefined()
    };

    if !using_iterator.is_undefined() {
        if !using_iterator.is_callable() {
            return Err(VmError::type_error(
                "TypedArray.from: @@iterator is not callable",
            ));
        }

        let iterator = ncx.call_function(&using_iterator, source.clone(), &[])?;
        let iter_obj = iterator
            .as_object()
            .ok_or_else(|| VmError::type_error("TypedArray.from: iterator is not an object"))?;
        loop {
            let next_fn =
                crate::object::get_value_full(&iter_obj, &PropertyKey::string("next"), ncx)?;
            if !next_fn.is_callable() {
                return Err(VmError::type_error(
                    "TypedArray.from: iterator.next is not callable",
                ));
            }
            let next_result = ncx.call_function(&next_fn, iterator.clone(), &[])?;
            let next_obj = next_result.as_object().ok_or_else(|| {
                VmError::type_error("TypedArray.from: iterator result is not an object")
            })?;
            let done = crate::object::get_value_full(&next_obj, &PropertyKey::string("done"), ncx)?
                .to_boolean();
            if done {
                break;
            }

            let value =
                crate::object::get_value_full(&next_obj, &PropertyKey::string("value"), ncx)?;
            values.push(value);
        }
    } else if let Some(obj) = source.as_object() {
        let len_value = crate::object::get_value_full(&obj, &PropertyKey::string("length"), ncx)?;
        let len_number = if let Some(n) = len_value.as_number() {
            n
        } else if let Some(i) = len_value.as_int32() {
            i as f64
        } else if len_value.is_undefined() {
            0.0
        } else {
            ncx.to_number_value(&len_value)?
        };
        let length = if len_number.is_nan() || len_number <= 0.0 {
            0
        } else {
            len_number.min(9007199254740991.0) as usize
        };
        values.reserve(length);
        for i in 0..length {
            let value = crate::object::get_value_full(&obj, &PropertyKey::Index(i as u32), ncx)?;
            values.push(value);
        }
    }

    let target = ncx.call_function_construct(
        this_val,
        Value::undefined(),
        &[Value::number(values.len() as f64)],
    )?;
    let target_obj = target.as_object().ok_or_else(|| {
        VmError::type_error("TypedArray.from: constructor did not return an object")
    })?;
    let ta_data = target_obj
        .get(&PropertyKey::string("__TypedArrayData__"))
        .ok_or_else(|| {
            VmError::type_error("TypedArray.from: constructor did not create a TypedArray")
        })?;
    let target_ta = ta_data.as_typed_array().ok_or_else(|| {
        VmError::type_error("TypedArray.from: constructor did not create a TypedArray")
    })?;

    if values.len() > target_ta.length() {
        return Err(VmError::type_error(
            "TypedArray.from: constructor returned a smaller TypedArray",
        ));
    }

    for (i, value) in values.into_iter().enumerate() {
        let mapped = if has_map {
            ncx.call_function(&map_fn, this_arg.clone(), &[value, Value::number(i as f64)])?
        } else {
            value
        };

        if target_ta.kind().is_bigint() {
            let bigint = if let Some(b) = mapped.as_bigint() {
                b.value
                    .parse::<i64>()
                    .map_err(|_| VmError::type_error("TypedArray.from: invalid BigInt value"))?
            } else {
                let prim = if mapped.is_object() {
                    ncx.to_primitive(&mapped, crate::interpreter::PreferredType::Number)?
                } else {
                    mapped.clone()
                };
                if let Some(b) = prim.as_bigint() {
                    b.value
                        .parse::<i64>()
                        .map_err(|_| VmError::type_error("TypedArray.from: invalid BigInt value"))?
                } else {
                    return Err(VmError::type_error("Cannot convert value to BigInt"));
                }
            };
            if !target_ta.set_bigint(i, bigint) {
                return Err(VmError::type_error(
                    "TypedArray.from: value does not fit in target typed array",
                ));
            }
            let _ = target_obj.set(
                PropertyKey::Index(i as u32),
                Value::bigint(bigint.to_string()),
            );
        } else {
            let number = ncx.to_number_value(&mapped)?;
            if !target_ta.set(i, number) {
                return Err(VmError::type_error(
                    "TypedArray.from: value does not fit in target typed array",
                ));
            }
            if let Some(stored) = target_ta.get(i) {
                let _ = target_obj.set(PropertyKey::Index(i as u32), Value::number(stored));
            }
        }
    }

    Ok(target)
}

fn typed_array_of_static(
    kind: TypedArrayKind,
    proto: GcRef<JsObject>,
) -> impl Fn(&Value, &[Value], &mut crate::context::NativeContext<'_>) -> Result<Value, VmError>
+ Send
+ Sync
+ 'static {
    move |_this, args, _ncx| {
        let length = args.len();
        let buffer = GcRef::new(JsArrayBuffer::new(length * kind.element_size(), None));
        let object = GcRef::new(JsObject::new(Value::object(proto)));
        let ta = JsTypedArray::new(object, buffer, kind, 0, length).map_err(VmError::type_error)?;
        for (i, arg) in args.iter().enumerate() {
            if let Some(num) = arg.as_number() {
                let _ = ta.set(i, num);
            } else if let Some(num_int) = arg.as_int32() {
                let _ = ta.set(i, num_int as f64);
            }
        }
        Ok(make_typed_array_object(ta))
    }
}

pub struct TypedArrayIntrinsic;

impl IntrinsicObject for TypedArrayIntrinsic {
    fn init(ctx: &IntrinsicContext) {
        let mm = ctx.mm();
        let intrinsics = ctx.intrinsics();

        init_typed_array_prototype(
            intrinsics.typed_array_prototype,
            ctx.fn_proto(),
            &mm,
            crate::intrinsics::well_known::iterator_symbol(),
            crate::intrinsics::well_known::to_string_tag_symbol(),
            intrinsics.array_iterator_prototype,
        );
        for (proto, kind) in [
            (intrinsics.int8_array_prototype, TypedArrayKind::Int8),
            (intrinsics.uint8_array_prototype, TypedArrayKind::Uint8),
            (
                intrinsics.uint8_clamped_array_prototype,
                TypedArrayKind::Uint8Clamped,
            ),
            (intrinsics.int16_array_prototype, TypedArrayKind::Int16),
            (intrinsics.uint16_array_prototype, TypedArrayKind::Uint16),
            (intrinsics.int32_array_prototype, TypedArrayKind::Int32),
            (intrinsics.uint32_array_prototype, TypedArrayKind::Uint32),
            (intrinsics.float32_array_prototype, TypedArrayKind::Float32),
            (intrinsics.float64_array_prototype, TypedArrayKind::Float64),
            (
                intrinsics.bigint64_array_prototype,
                TypedArrayKind::BigInt64,
            ),
            (
                intrinsics.biguint64_array_prototype,
                TypedArrayKind::BigUint64,
            ),
        ] {
            init_specific_typed_array_prototype(
                proto,
                kind,
                crate::intrinsics::well_known::to_string_tag_symbol(),
            );
        }
        init_array_buffer_prototype(
            intrinsics.array_buffer_prototype,
            ctx.fn_proto(),
            &mm,
            crate::intrinsics::well_known::to_string_tag_symbol(),
        );
        init_data_view_prototype(
            intrinsics.data_view_prototype,
            ctx.fn_proto(),
            &mm,
            crate::intrinsics::well_known::to_string_tag_symbol(),
        );

        if let Some(global) = ctx.global_opt() {
            let array_buffer_ctor = ctx.alloc_constructor();
            BuiltInBuilder::new(
                mm.clone(),
                ctx.fn_proto(),
                array_buffer_ctor,
                intrinsics.array_buffer_prototype,
                "ArrayBuffer",
            )
            .inherits(ctx.obj_proto())
            .constructor_fn(
                |this, args: &[Value], _ncx| {
                    let len = if let Some(arg) = args.first() {
                        let n = crate::globals::to_number(arg);
                        if n.is_nan() || n < 0.0 || n > 1_073_741_824.0 {
                            return Err(VmError::range_error("Invalid array buffer length"));
                        }
                        n as usize
                    } else {
                        0
                    };

                    let proto = this.as_object().map(|o| o.prototype());
                    let ab = GcRef::new(JsArrayBuffer::new(len, None));
                    if let Some(p) = proto {
                        ab.object.set_prototype(p);
                    }
                    Ok(Value::array_buffer(ab))
                },
                1,
            )
            .build_and_install(&global);
            array_buffer_ctor.define_property(
                PropertyKey::string("isView"),
                PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                    |_this, args, _ncx| {
                        Ok(Value::boolean(
                            args.first()
                                .map(|arg| arg.is_typed_array() || arg.is_data_view())
                                .unwrap_or(false),
                        ))
                    },
                    mm.clone(),
                    ctx.fn_proto(),
                )),
            );

            let data_view_ctor = ctx.alloc_constructor();
            BuiltInBuilder::new(
                mm.clone(),
                ctx.fn_proto(),
                data_view_ctor,
                intrinsics.data_view_prototype,
                "DataView",
            )
            .inherits(ctx.obj_proto())
            .constructor_fn(
                |_this, args: &[Value], _ncx| {
                    let first_arg = args.first().cloned().unwrap_or(Value::undefined());
                    let buffer = first_arg.as_array_buffer().ok_or_else(|| {
                        VmError::type_error(
                            "First argument to DataView constructor must be an ArrayBuffer",
                        )
                    })?;

                    let byte_offset = if let Some(v) = args.get(1) {
                        if v.is_undefined() {
                            0usize
                        } else {
                            let n = crate::globals::to_number(v);
                            if n.is_nan() || n < 0.0 || n.is_infinite() || n != n.trunc() {
                                return Err(VmError::range_error("Invalid byte offset"));
                            }
                            n as usize
                        }
                    } else {
                        0
                    };

                    let byte_length = if let Some(v) = args.get(2) {
                        if v.is_undefined() {
                            None
                        } else {
                            let n = crate::globals::to_number(v);
                            if n.is_nan() || n < 0.0 || n.is_infinite() {
                                return Err(VmError::range_error("Invalid byte length"));
                            }
                            Some(n as usize)
                        }
                    } else {
                        None
                    };

                    let dv = JsDataView::new(buffer.clone(), byte_offset, byte_length)
                        .map_err(VmError::range_error)?;
                    Ok(Value::data_view(GcRef::new(dv)))
                },
                1,
            )
            .build_and_install(&global);

            let typed_array_ctor = Value::native_function_with_proto(
                |_this, _args, _ncx| Err(VmError::type_error("TypedArray constructor is abstract")),
                mm.clone(),
                ctx.fn_proto(),
            );
            if let Some(typed_array_ctor_obj) = typed_array_ctor.as_object() {
                typed_array_ctor_obj.define_property(
                    PropertyKey::string("__builtin_tag__"),
                    PropertyDescriptor::data_with_attrs(
                        Value::string(JsString::intern("TypedArray")),
                        PropertyAttributes::permanent(),
                    ),
                );
                typed_array_ctor_obj.define_property(
                    PropertyKey::string("from"),
                    PropertyDescriptor::builtin_method(Value::native_function_with_proto_named(
                        typed_array_from_static,
                        mm.clone(),
                        ctx.fn_proto(),
                        "from",
                        1,
                    )),
                );

                for (kind, name, proto) in [
                    (
                        TypedArrayKind::Int8,
                        "Int8Array",
                        intrinsics.int8_array_prototype,
                    ),
                    (
                        TypedArrayKind::Uint8,
                        "Uint8Array",
                        intrinsics.uint8_array_prototype,
                    ),
                    (
                        TypedArrayKind::Uint8Clamped,
                        "Uint8ClampedArray",
                        intrinsics.uint8_clamped_array_prototype,
                    ),
                    (
                        TypedArrayKind::Int16,
                        "Int16Array",
                        intrinsics.int16_array_prototype,
                    ),
                    (
                        TypedArrayKind::Uint16,
                        "Uint16Array",
                        intrinsics.uint16_array_prototype,
                    ),
                    (
                        TypedArrayKind::Int32,
                        "Int32Array",
                        intrinsics.int32_array_prototype,
                    ),
                    (
                        TypedArrayKind::Uint32,
                        "Uint32Array",
                        intrinsics.uint32_array_prototype,
                    ),
                    (
                        TypedArrayKind::Float32,
                        "Float32Array",
                        intrinsics.float32_array_prototype,
                    ),
                    (
                        TypedArrayKind::Float64,
                        "Float64Array",
                        intrinsics.float64_array_prototype,
                    ),
                    (
                        TypedArrayKind::BigInt64,
                        "BigInt64Array",
                        intrinsics.bigint64_array_prototype,
                    ),
                    (
                        TypedArrayKind::BigUint64,
                        "BigUint64Array",
                        intrinsics.biguint64_array_prototype,
                    ),
                ] {
                    let ctor = ctx.alloc_constructor();
                    BuiltInBuilder::new(mm.clone(), ctx.fn_proto(), ctor, proto, name)
                        .inherits(intrinsics.typed_array_prototype)
                        .constructor_fn(create_typed_array_constructor(kind, proto), 3)
                        .build_and_install(&global);
                    ctor.set_prototype(Value::object(typed_array_ctor_obj));
                    ctor.define_property(
                        PropertyKey::string("BYTES_PER_ELEMENT"),
                        PropertyDescriptor::builtin_data(Value::int32(kind.element_size() as i32)),
                    );
                    ctor.define_property(
                        PropertyKey::string("of"),
                        PropertyDescriptor::builtin_method(Value::native_function_with_proto(
                            typed_array_of_static(kind, proto),
                            mm.clone(),
                            ctx.fn_proto(),
                        )),
                    );
                }
            }
        }
    }
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
        let buffer = GcRef::new(JsArrayBuffer::new(byte_len, None));
        let object = GcRef::new(JsObject::new(proto));
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
            if let Some(b) = val.as_bigint() {
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
