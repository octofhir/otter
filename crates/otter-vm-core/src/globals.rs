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

use crate::array_buffer::JsArrayBuffer;
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
    let fn_obj = GcRef::new(JsObject::new(Value::object(fn_proto), mm.clone()));
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

/// Set up all standard global properties on the global object.
///
/// `fn_proto` is the intrinsic `%Function.prototype%` created by VmRuntime.
/// All native functions receive it as their `[[Prototype]]` per ES2023 §10.3.1.
/// `intrinsics_opt` is optional intrinsics for TypedArray prototypes.
pub fn setup_global_object(
    global: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    intrinsics_opt: Option<&crate::intrinsics::Intrinsics>,
) {
    let mm = global.memory_manager().clone();

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

    // Standard built-in objects
    setup_builtin_constructors(global, fn_proto, intrinsics_opt);
}

/// Set up standard built-in constructors and their prototypes.
/// `fn_proto` is the intrinsic `%Function.prototype%` — used as-is for `Function.prototype`
/// and as `[[Prototype]]` for all native function objects.
/// `intrinsics_opt` is optional intrinsics for TypedArray prototypes.
fn setup_builtin_constructors(
    global: GcRef<JsObject>,
    fn_proto: GcRef<JsObject>,
    intrinsics_opt: Option<&crate::intrinsics::Intrinsics>,
) {
    let mm = global.memory_manager().clone();
    let tag_builtin = |ctor: &Value, name: &str| {
        if let Some(obj) = ctor.as_object() {
            obj.define_property(
                PropertyKey::string("__builtin_tag__"),
                PropertyDescriptor::data_with_attrs(
                    Value::string(JsString::intern(name)),
                    PropertyAttributes::permanent(),
                ),
            );
        }
    };
    let is_typed_array_ctor_name = |name: &str| {
        matches!(
            name,
            "Int8Array"
                | "Uint8Array"
                | "Uint8ClampedArray"
                | "Int16Array"
                | "Uint16Array"
                | "Int32Array"
                | "Uint32Array"
                | "Float32Array"
                | "Float64Array"
                | "BigInt64Array"
                | "BigUint64Array"
        )
    };

    // Shared %TypedArray% constructor object for constructor inheritance:
    // Object.getPrototypeOf(Int8Array) === %TypedArray%.
    let typed_array_ctor = Value::native_function_with_proto(
        |_this, _args, _ncx| Err(VmError::type_error("TypedArray constructor is abstract")),
        mm.clone(),
        fn_proto,
    );
    tag_builtin(&typed_array_ctor, "TypedArray");
    if let Some(typed_array_ctor_obj) = typed_array_ctor.as_object() {
        let _ = typed_array_ctor_obj.set(
            PropertyKey::string("from"),
            Value::native_function_with_proto_named(
                move |this_val, args, ncx| {
                    let source = args.first().ok_or_else(|| {
                        VmError::type_error("TypedArray.from requires a source argument")
                    })?;
                    let map_fn = args.get(1).cloned().unwrap_or(Value::undefined());
                    let this_arg = args.get(2).cloned().unwrap_or(Value::undefined());
                    let has_map = !map_fn.is_undefined();
                    if has_map && !map_fn.is_callable() {
                        return Err(VmError::type_error(
                            "TypedArray.from: mapFn is not a function",
                        ));
                    }

                    // %TypedArray%.from requires a constructor as `this`.
                    let this_obj = this_val.as_object().ok_or_else(|| {
                        VmError::type_error("TypedArray.from: this is not a constructor")
                    })?;
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

                    // Collect source values first (iterable path preferred).
                    // mapFn is applied later during writes into targetObj.
                    let mut values: Vec<Value> = Vec::new();
                    let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
                    let using_iterator = if let Some(obj) = source.as_object() {
                        crate::object::get_value_full(
                            &obj,
                            &PropertyKey::Symbol(iterator_sym),
                            ncx,
                        )?
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
                        let iter_obj = iterator.as_object().ok_or_else(|| {
                            VmError::type_error("TypedArray.from: iterator is not an object")
                        })?;
                        loop {
                            let next_fn = crate::object::get_value_full(
                                &iter_obj,
                                &PropertyKey::string("next"),
                                ncx,
                            )?;
                            if !next_fn.is_callable() {
                                return Err(VmError::type_error(
                                    "TypedArray.from: iterator.next is not callable",
                                ));
                            }
                            let next_result = ncx.call_function(&next_fn, iterator.clone(), &[])?;
                            let next_obj = next_result.as_object().ok_or_else(|| {
                                VmError::type_error(
                                    "TypedArray.from: iterator result is not an object",
                                )
                            })?;
                            let done = crate::object::get_value_full(
                                &next_obj,
                                &PropertyKey::string("done"),
                                ncx,
                            )?
                            .to_boolean();
                            if done {
                                break;
                            }

                            let value = crate::object::get_value_full(
                                &next_obj,
                                &PropertyKey::string("value"),
                                ncx,
                            )?;
                            values.push(value);
                        }
                    } else if let Some(obj) = source.as_object() {
                        let len_value = crate::object::get_value_full(
                            &obj,
                            &PropertyKey::string("length"),
                            ncx,
                        )?;
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
                            let value = crate::object::get_value_full(
                                &obj,
                                &PropertyKey::Index(i as u32),
                                ncx,
                            )?;
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
                            VmError::type_error(
                                "TypedArray.from: constructor did not create a TypedArray",
                            )
                        })?;
                    let target_ta = ta_data.as_typed_array().ok_or_else(|| {
                        VmError::type_error(
                            "TypedArray.from: constructor did not create a TypedArray",
                        )
                    })?;

                    if values.len() > target_ta.length() {
                        return Err(VmError::type_error(
                            "TypedArray.from: constructor returned a smaller TypedArray",
                        ));
                    }

                    for (i, value) in values.into_iter().enumerate() {
                        let mapped = if has_map {
                            ncx.call_function(
                                &map_fn,
                                this_arg.clone(),
                                &[value, Value::number(i as f64)],
                            )?
                        } else {
                            value
                        };

                        if target_ta.kind().is_bigint() {
                            let bigint = if let Some(crate::value::HeapRef::BigInt(b)) =
                                mapped.heap_ref()
                            {
                                b.value.parse::<i64>().map_err(|_| {
                                    VmError::type_error("TypedArray.from: invalid BigInt value")
                                })?
                            } else {
                                let prim = if mapped.is_object() {
                                    ncx.to_primitive(
                                        &mapped,
                                        crate::interpreter::PreferredType::Number,
                                    )?
                                } else {
                                    mapped.clone()
                                };
                                if let Some(crate::value::HeapRef::BigInt(b)) = prim.heap_ref() {
                                    b.value.parse::<i64>().map_err(|_| {
                                        VmError::type_error("TypedArray.from: invalid BigInt value")
                                    })?
                                } else {
                                    return Err(VmError::type_error(
                                        "Cannot convert value to BigInt",
                                    ));
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
                                let _ = target_obj
                                    .set(PropertyKey::Index(i as u32), Value::number(stored));
                            }
                        }
                    }

                    Ok(target)
                },
                mm.clone(),
                fn_proto,
                "from",
                1,
            ),
        );
    }
    let builtins = [
        "Object",
        "Function",
        "Array",
        "String",
        "Number",
        "Boolean",
        "RegExp",
        "Error",
        "TypeError",
        "ReferenceError",
        "SyntaxError",
        "RangeError",
        "URIError",
        "EvalError",
        "Date",
        "BigInt",
        "Test262Error",
        "Map",
        "Set",
        "WeakMap",
        "WeakSet",
        "Promise",
        "Proxy",
        "Symbol",
        "GeneratorPrototype",
        "IteratorPrototype",
        "AsyncIteratorPrototype",
        "AsyncGeneratorPrototype",
        "ArrayBuffer",
        "DataView",
        "Int8Array",
        "Uint8Array",
        "Uint8ClampedArray",
        "Int16Array",
        "Uint16Array",
        "Int32Array",
        "Uint32Array",
        "Float32Array",
        "Float64Array",
        "BigInt64Array",
        "BigUint64Array",
        "AbortController",
        "AbortSignal",
    ];

    for name in builtins {
        // For the "Function" constructor, use the intrinsic fn_proto
        // instead of creating a fresh object.
        // Function.prototype is created once and shared.
        // For TypedArrays, use intrinsic prototypes if available.
        let proto = if name == "Function" {
            fn_proto
        } else if let Some(intrinsics) = intrinsics_opt {
            match name {
                "Int8Array" => intrinsics.int8_array_prototype,
                "Uint8Array" => intrinsics.uint8_array_prototype,
                "Uint8ClampedArray" => intrinsics.uint8_clamped_array_prototype,
                "Int16Array" => intrinsics.int16_array_prototype,
                "Uint16Array" => intrinsics.uint16_array_prototype,
                "Int32Array" => intrinsics.int32_array_prototype,
                "Uint32Array" => intrinsics.uint32_array_prototype,
                "Float32Array" => intrinsics.float32_array_prototype,
                "Float64Array" => intrinsics.float64_array_prototype,
                "BigInt64Array" => intrinsics.bigint64_array_prototype,

                "BigUint64Array" => intrinsics.biguint64_array_prototype,
                "AbortController" => intrinsics.abort_controller_prototype,
                "AbortSignal" => intrinsics.abort_signal_prototype,
                _ => GcRef::new(JsObject::new(Value::null(), mm.clone())),
            }
        } else {
            GcRef::new(JsObject::new(Value::null(), mm.clone()))
        };

        // Create constructor based on type — all get fn_proto as [[Prototype]]
        let ctor = if let Some(intrinsics) =
            intrinsics_opt.filter(|_i| matches!(name, "AbortController" | "AbortSignal"))
        {
            match name {
                "AbortController" => Value::native_function_with_proto_and_object(
                    Arc::new(crate::web_api::abort_controller::AbortController::constructor),
                    mm.clone(),
                    fn_proto,
                    intrinsics.abort_controller_constructor,
                ),
                "AbortSignal" => Value::native_function_with_proto_and_object(
                    Arc::new(|_, _, _| {
                        Err(VmError::type_error(
                            "Constructing an AbortSignal manually is not supported",
                        ))
                    }),
                    mm.clone(),
                    fn_proto,
                    intrinsics.abort_signal_constructor,
                ),
                _ => unreachable!(),
            }
        } else if name == "Boolean" {
            Value::native_function_with_proto(
                |_this, args: &[Value], _ncx| {
                    let b = if let Some(val) = args.get(0) {
                        to_boolean(val)
                    } else {
                        false // to_boolean(undefined) is false
                    };
                    Ok(Value::boolean(b))
                },
                mm.clone(),
                fn_proto,
            )
        } else if name == "BigInt" {
            Value::native_function_with_proto(
                |_this, args: &[Value], _ncx| {
                    if let Some(val) = args.get(0) {
                        if let Some(n) = val.as_number() {
                            if n.is_nan() || n.is_infinite() {
                                return Err(VmError::type_error("RangeError: invalid BigInt"));
                            }
                            if n.trunc() != n {
                                return Err(VmError::type_error(
                                    "RangeError: The number cannot be converted to a BigInt because it is not an integer",
                                ));
                            }
                            return Ok(Value::bigint(format!("{:.0}", n)));
                        }
                        if val.is_string() {
                            let s = to_string(val);
                            return Ok(Value::bigint(s));
                        }
                        if val.is_boolean() {
                            return Ok(Value::bigint(if val.to_boolean() {
                                "1".to_string()
                            } else {
                                "0".to_string()
                            }));
                        }
                        // Fallback
                        let s = to_string(val);
                        Ok(Value::bigint(s))
                    } else {
                        Err(VmError::type_error(
                            "TypeError: Cannot convert undefined to a BigInt",
                        ))
                    }
                },
                mm.clone(),
                fn_proto,
            )
        } else if name == "ArrayBuffer" {
            let mm_clone = mm.clone();
            Value::native_function_with_proto(
                move |_this, args: &[Value], ncx| {
                    let len = if let Some(arg) = args.get(0) {
                        let n = to_number(arg);
                        if n.is_nan() || n < 0.0 || n > 1_073_741_824.0 {
                            return Err(VmError::range_error("Invalid array buffer length"));
                        }
                        n as usize
                    } else {
                        0
                    };

                    let ab = GcRef::new(JsArrayBuffer::new(
                        len,
                        Some(fn_proto),
                        ncx.memory_manager().clone(),
                    ));
                    Ok(Value::array_buffer(ab))
                },
                mm_clone,
                fn_proto,
            )
        } else if name.ends_with("Array")
            && (name == "Int8Array"
                || name == "Uint8Array"
                || name == "Uint8ClampedArray"
                || name == "Int16Array"
                || name == "Uint16Array"
                || name == "Int32Array"
                || name == "Uint32Array"
                || name == "Float32Array"
                || name == "Float64Array"
                || name == "BigInt64Array"
                || name == "BigUint64Array")
        {
            // TypedArray constructors
            use crate::array_buffer::JsArrayBuffer;
            use crate::typed_array::{JsTypedArray, TypedArrayKind};

            let kind = match name {
                "Int8Array" => TypedArrayKind::Int8,
                "Uint8Array" => TypedArrayKind::Uint8,
                "Uint8ClampedArray" => TypedArrayKind::Uint8Clamped,
                "Int16Array" => TypedArrayKind::Int16,
                "Uint16Array" => TypedArrayKind::Uint16,
                "Int32Array" => TypedArrayKind::Int32,
                "Uint32Array" => TypedArrayKind::Uint32,
                "Float32Array" => TypedArrayKind::Float32,
                "Float64Array" => TypedArrayKind::Float64,
                "BigInt64Array" => TypedArrayKind::BigInt64,
                "BigUint64Array" => TypedArrayKind::BigUint64,
                _ => unreachable!(),
            };

            let mm_clone = mm.clone();
            let proto_clone = proto;
            Value::native_function_with_proto(
                move |_this, args: &[Value], ncx| {
                    // Helper to create TypedArray with hidden property for getter access
                    let make_typed_array = |ta: JsTypedArray| -> Value {
                        let ta_arc = GcRef::new(ta);
                        let obj = ta_arc.object;
                        // Store TypedArray as hidden property so getters can access it
                        obj.define_property(
                            PropertyKey::string("__TypedArrayData__"),
                            PropertyDescriptor::data(Value::typed_array(ta_arc)),
                        );
                        // Return the object directly, not the TypedArray value
                        Value::object(obj)
                    };

                    // Handle 4 constructor forms:
                    // new TypedArray() - create empty
                    // new TypedArray(length)
                    // new TypedArray(typedArray)
                    // new TypedArray(buffer[, byteOffset[, length]])
                    // new TypedArray(arrayLike)

                    if args.is_empty() {
                        // new TypedArray() - create empty with length 0
                        let buffer =
                            GcRef::new(JsArrayBuffer::new(0, None, ncx.memory_manager().clone()));
                        let object = GcRef::new(JsObject::new(
                            Value::object(proto_clone),
                            ncx.memory_manager().clone(),
                        ));
                        let ta = JsTypedArray::new(object, buffer, kind, 0, 0)
                            .map_err(|e| VmError::type_error(e))?;
                        return Ok(make_typed_array(ta));
                    }

                    let arg0 = &args[0];

                    // Check if arg0 is ArrayBuffer
                    if let Some(buffer) = arg0.as_array_buffer() {
                        let byte_offset =
                            args.get(1).and_then(|v| v.as_int32()).unwrap_or(0) as usize;

                        let length = if let Some(len_val) = args.get(2) {
                            len_val.as_int32().unwrap_or(0) as usize
                        } else {
                            // Auto-calculate length from buffer
                            let available = buffer.byte_length().saturating_sub(byte_offset);
                            available / kind.element_size()
                        };

                        let object = GcRef::new(JsObject::new(
                            Value::object(proto_clone),
                            ncx.memory_manager().clone(),
                        ));
                        let ta =
                            JsTypedArray::new(object, buffer.clone(), kind, byte_offset, length)
                                .map_err(|e| VmError::type_error(e))?;
                        return Ok(make_typed_array(ta));
                    }

                    // Check if arg0 is another TypedArray
                    if let Some(other_ta) = arg0.as_typed_array() {
                        let length = other_ta.length();
                        let byte_len = length
                            .checked_mul(kind.element_size())
                            .ok_or_else(|| VmError::range_error("Invalid typed array length"))?;
                        let buffer = GcRef::new(JsArrayBuffer::new(
                            byte_len,
                            None,
                            ncx.memory_manager().clone(),
                        ));
                        let object = GcRef::new(JsObject::new(
                            Value::object(proto_clone),
                            ncx.memory_manager().clone(),
                        ));
                        let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                            .map_err(|e| VmError::type_error(e))?;

                        // Copy elements
                        for i in 0..length {
                            if let Some(val) = other_ta.get(i) {
                                let _ = ta.set(i, val);
                            }
                        }

                        return Ok(make_typed_array(ta));
                    }

                    // Check if arg0 is a number (length)
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
                        let buffer = GcRef::new(JsArrayBuffer::new(
                            byte_len,
                            None,
                            ncx.memory_manager().clone(),
                        ));
                        let object = GcRef::new(JsObject::new(
                            Value::object(proto_clone),
                            ncx.memory_manager().clone(),
                        ));
                        let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                            .map_err(|e| VmError::type_error(e))?;
                        return Ok(make_typed_array(ta));
                    }

                    if let Some(length_int) = arg0.as_int32() {
                        if length_int < 0 {
                            return Err(VmError::range_error("Invalid typed array length"));
                        }
                        let length = length_int as usize;
                        let byte_len = length
                            .checked_mul(kind.element_size())
                            .ok_or_else(|| VmError::range_error("Invalid typed array length"))?;
                        let buffer = GcRef::new(JsArrayBuffer::new(
                            byte_len,
                            None,
                            ncx.memory_manager().clone(),
                        ));
                        let object = GcRef::new(JsObject::new(
                            Value::object(proto_clone),
                            ncx.memory_manager().clone(),
                        ));
                        let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                            .map_err(|e| VmError::type_error(e))?;
                        return Ok(make_typed_array(ta));
                    }

                    // Array-like object
                    if let Some(obj) = arg0.as_object() {
                        let iterator_sym = crate::intrinsics::well_known::iterator_symbol();
                        if let Some(iter_fn) = obj.get(&PropertyKey::Symbol(iterator_sym))
                            && iter_fn.is_callable()
                        {
                            let iterator = ncx.call_function(&iter_fn, arg0.clone(), &[])?;
                            let iter_obj = iterator.as_object().ok_or_else(|| {
                                VmError::type_error(
                                    "TypedArray constructor: iterator is not an object",
                                )
                            })?;
                            let mut values: Vec<Value> = Vec::new();
                            loop {
                                let next_fn = iter_obj
                                    .get(&PropertyKey::string("next"))
                                    .ok_or_else(|| {
                                        VmError::type_error(
                                            "TypedArray constructor: iterator.next is not defined",
                                        )
                                    })?;
                                if !next_fn.is_callable() {
                                    return Err(VmError::type_error(
                                        "TypedArray constructor: iterator.next is not callable",
                                    ));
                                }
                                let next_result =
                                    ncx.call_function(&next_fn, iterator.clone(), &[])?;
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
                            let byte_len =
                                length.checked_mul(kind.element_size()).ok_or_else(|| {
                                    VmError::range_error("Invalid typed array length")
                                })?;
                            let buffer = GcRef::new(JsArrayBuffer::new(
                                byte_len,
                                None,
                                ncx.memory_manager().clone(),
                            ));
                            let object = GcRef::new(JsObject::new(
                                Value::object(proto_clone),
                                ncx.memory_manager().clone(),
                            ));
                            let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                                .map_err(|e| VmError::type_error(e))?;

                            for (i, val) in values.into_iter().enumerate() {
                                if kind.is_bigint() {
                                    if let Some(crate::value::HeapRef::BigInt(b)) = val.heap_ref()
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

                            return Ok(make_typed_array(ta));
                        }

                        if let Some(length_val) = obj.get(&PropertyKey::string("length")) {
                            if let Some(length) = length_val.as_int32() {
                                let length = length.max(0) as usize;
                                let buffer = GcRef::new(JsArrayBuffer::new(
                                    length * kind.element_size(),
                                    None,
                                    ncx.memory_manager().clone(),
                                ));
                                let object = GcRef::new(JsObject::new(
                                    Value::object(proto_clone),
                                    ncx.memory_manager().clone(),
                                ));
                                let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                                    .map_err(|e| VmError::type_error(e))?;

                                // Copy elements from object
                                for i in 0..length {
                                    if let Some(val) = obj.get(&PropertyKey::Index(i as u32)) {
                                        if let Some(num) = val.as_number() {
                                            let _ = ta.set(i, num);
                                        } else if let Some(num_int) = val.as_int32() {
                                            let _ = ta.set(i, num_int as f64);
                                        }
                                    }
                                }

                                return Ok(make_typed_array(ta));
                            }
                        }
                    }

                    // Default: treat as length 0
                    let buffer =
                        GcRef::new(JsArrayBuffer::new(0, None, ncx.memory_manager().clone()));
                    let object = GcRef::new(JsObject::new(
                        Value::object(proto_clone),
                        ncx.memory_manager().clone(),
                    ));
                    let ta = JsTypedArray::new(object, buffer, kind, 0, 0)
                        .map_err(|e| VmError::type_error(e))?;
                    Ok(make_typed_array(ta))
                },
                mm_clone,
                fn_proto,
            )
        } else if name == "Proxy" {
            // Proxy constructor
            Value::native_function_with_proto(
                crate::intrinsics_impl::proxy::proxy_constructor,
                mm.clone(),
                fn_proto,
            )
        } else {
            let mm_clone = mm.clone();
            Value::native_function_with_proto(
                move |_this, args: &[Value], ncx| {
                    // If called as a constructor (which we assume for now for these builtins),
                    // and arguments are present, we might want to set properties.
                    // For Error types, setting 'message' is crucial.
                    if let Some(msg) = args.get(0) {
                        let obj = JsObject::new(Value::null(), ncx.memory_manager().clone());
                        let _ = obj.set(PropertyKey::string("message"), msg.clone());
                        return Ok(Value::object(GcRef::new(obj)));
                    }
                    Ok(Value::undefined())
                },
                mm_clone,
                fn_proto,
            )
        };

        tag_builtin(&ctor, name);

        // Add basic toString to prototypes
        if name == "Object" {
            let _ = proto.set(
                PropertyKey::string("toString"),
                Value::native_function_with_proto(
                    |_this, _, _ncx| Ok(Value::string(JsString::intern("[object Object]"))),
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "Function" {
            let _ = proto.set(
                PropertyKey::string("toString"),
                Value::native_function_with_proto(
                    |_this, _, _ncx| {
                        Ok(Value::string(JsString::intern(
                            "function () { [native code] }",
                        )))
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "String" {
            let _ = proto.set(
                PropertyKey::string("toString"),
                Value::native_function_with_proto(
                    |this_val, _args, _ncx| {
                        Ok(Value::string(JsString::intern(&to_string(this_val))))
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
        }

        if let Some(ctor_obj) = ctor.as_object() {
            if is_typed_array_ctor_name(name) {
                if let Some(parent_ta_ctor) = typed_array_ctor.as_object() {
                    ctor_obj.set_prototype(Value::object(parent_ta_ctor));
                }
                // These native typed array constructors are constructible in this runtime.
                let _ = ctor_obj.set(
                    PropertyKey::string("__non_constructor"),
                    Value::boolean(false),
                );
            } else if name == "Proxy" {
                // Proxy is constructible (`new Proxy(...)`) but intentionally has no `.prototype`.
                let _ = ctor_obj.set(
                    PropertyKey::string("__non_constructor"),
                    Value::boolean(false),
                );
            }

            if name != "Proxy" {
                let _ = ctor_obj.set(
                    PropertyKey::string("prototype"),
                    Value::object(proto.clone()),
                );
                let _ = proto.set(PropertyKey::string("constructor"), ctor.clone());
            }

            // Add static methods to constructors
            if name == "String" {
                let _ = ctor_obj.set(
                    PropertyKey::string("fromCharCode"),
                    Value::native_function_with_proto(
                        |_this, args: &[Value], _ncx| {
                            let mut result = String::new();
                            for arg in args {
                                // Per ES2023 §22.1.2.1: ToUint16(ToNumber(arg))
                                let n = if let Some(n) = arg.as_number() {
                                    n
                                } else if let Some(i) = arg.as_int32() {
                                    i as f64
                                } else if let Some(s) = arg.as_string() {
                                    let trimmed = s.as_str().trim();
                                    if trimmed.is_empty() {
                                        0.0
                                    } else {
                                        trimmed.parse::<f64>().unwrap_or(f64::NAN)
                                    }
                                } else if let Some(b) = arg.as_boolean() {
                                    if b { 1.0 } else { 0.0 }
                                } else if arg.is_null() {
                                    0.0
                                } else {
                                    f64::NAN
                                };
                                let code = if n.is_nan() || n.is_infinite() {
                                    0u16
                                } else {
                                    (n.trunc() as i64 as u32 & 0xFFFF) as u16
                                };
                                if let Some(c) = std::char::from_u32(code as u32) {
                                    result.push(c);
                                }
                            }
                            Ok(Value::string(JsString::intern(&result)))
                        },
                        mm.clone(),
                        fn_proto,
                    ),
                );
            } else if name == "ArrayBuffer" {
                let _ = ctor_obj.set(
                    PropertyKey::string("isView"),
                    Value::native_function_with_proto(
                        |_this, args, _ncx| {
                            if let Some(arg) = args.get(0) {
                                Ok(Value::boolean(arg.is_typed_array() || arg.is_data_view()))
                            } else {
                                Ok(Value::boolean(false))
                            }
                        },
                        mm.clone(),
                        fn_proto,
                    ),
                );
            } else if name.ends_with("Array")
                && (name == "Int8Array"
                    || name == "Uint8Array"
                    || name == "Uint8ClampedArray"
                    || name == "Int16Array"
                    || name == "Uint16Array"
                    || name == "Int32Array"
                    || name == "Uint32Array"
                    || name == "Float32Array"
                    || name == "Float64Array"
                    || name == "BigInt64Array"
                    || name == "BigUint64Array")
            {
                // Add TypedArray static methods and properties
                use crate::array_buffer::JsArrayBuffer;
                use crate::typed_array::{JsTypedArray, TypedArrayKind};

                let kind = match name {
                    "Int8Array" => TypedArrayKind::Int8,
                    "Uint8Array" => TypedArrayKind::Uint8,
                    "Uint8ClampedArray" => TypedArrayKind::Uint8Clamped,
                    "Int16Array" => TypedArrayKind::Int16,
                    "Uint16Array" => TypedArrayKind::Uint16,
                    "Int32Array" => TypedArrayKind::Int32,
                    "Uint32Array" => TypedArrayKind::Uint32,
                    "Float32Array" => TypedArrayKind::Float32,
                    "Float64Array" => TypedArrayKind::Float64,
                    "BigInt64Array" => TypedArrayKind::BigInt64,
                    "BigUint64Array" => TypedArrayKind::BigUint64,
                    _ => unreachable!(),
                };

                // BYTES_PER_ELEMENT - ES2026 §22.2.5.1
                let _ = ctor_obj.set(
                    PropertyKey::string("BYTES_PER_ELEMENT"),
                    Value::int32(kind.element_size() as i32),
                );

                // TypedArray.of(...items) - ES2026 §22.2.2.2
                let mm_of = mm.clone();
                let proto_of = proto.clone();
                let _ = ctor_obj.set(
                    PropertyKey::string("of"),
                    Value::native_function_with_proto(
                        move |_this, args, ncx| {
                            let length = args.len();

                            // Create new TypedArray
                            let buffer = GcRef::new(JsArrayBuffer::new(
                                length * kind.element_size(),
                                None,
                                ncx.memory_manager().clone(),
                            ));
                            let object = GcRef::new(JsObject::new(
                                Value::object(proto_of),
                                ncx.memory_manager().clone(),
                            ));
                            let ta = JsTypedArray::new(object, buffer, kind, 0, length)
                                .map_err(|e| VmError::type_error(e))?;

                            // Set elements from arguments
                            for (i, arg) in args.iter().enumerate() {
                                if let Some(num) = arg.as_number() {
                                    let _ = ta.set(i, num);
                                } else if let Some(num_int) = arg.as_int32() {
                                    let _ = ta.set(i, num_int as f64);
                                }
                            }

                            let ta_arc = GcRef::new(ta);
                            object.define_property(
                                PropertyKey::string("__TypedArrayData__"),
                                PropertyDescriptor::data(Value::typed_array(ta_arc)),
                            );
                            Ok(Value::object(object))
                        },
                        mm_of,
                        fn_proto,
                    ),
                );
            }
        }

        // Add more prototype methods
        if name == "String" {
            let _ = proto.set(
                PropertyKey::string("indexOf"),
                Value::native_function_with_proto(
                    |this_val, args, _ncx| {
                        if let Some(search_val) = args.get(0) {
                            let this_str = to_string(this_val);
                            let search_str = to_string(search_val);
                            if let Some(pos) = this_str.find(&search_str) {
                                return Ok(Value::number(pos as f64));
                            }
                        }
                        Ok(Value::number(-1.0))
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
            let _ = proto.set(
                PropertyKey::string("valueOf"),
                Value::native_function_with_proto(
                    |this_val, _args, _ncx| Ok::<Value, VmError>(this_val.clone()),
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "Object" {
            let _ = proto.set(
                PropertyKey::string("valueOf"),
                Value::native_function_with_proto(
                    |this_val, _args, _ncx| Ok::<Value, VmError>(this_val.clone()),
                    mm.clone(),
                    fn_proto,
                ),
            );
        } else if name == "ArrayBuffer" {
            // ArrayBuffer.prototype.byteLength getter
            proto.define_property(
                PropertyKey::string("byteLength"),
                PropertyDescriptor::getter(Value::native_function_with_proto(
                    |this_val, _args, _ncx| {
                        if let Some(this) = this_val.as_array_buffer() {
                            Ok(Value::number(this.byte_length() as f64))
                        } else {
                             Err(VmError::type_error("TypeError: ArrayBuffer.prototype.byteLength called on incompatible receiver"))
                        }
                    },
                    mm.clone(),
                    fn_proto,
                )),
            );

            // ArrayBuffer.prototype.slice
            let _ = proto.set(
                PropertyKey::string("slice"),
                Value::native_function_with_proto(
                    |this_val, args, _ncx| {
                        let ab = this_val.as_array_buffer()
                            .ok_or("TypeError: ArrayBuffer.prototype.slice called on incompatible receiver")?;
                        let len = ab.byte_length() as f64;
                        let start_arg = to_number(args.get(0).unwrap_or(&Value::undefined()));
                        let start = if start_arg.is_nan() {
                            0
                        } else if start_arg < 0.0 {
                            (len + start_arg).max(0.0) as usize
                        } else {
                            start_arg.min(len) as usize
                        };

                        let end_arg = args.get(1).map(to_number).unwrap_or(len);
                        let end = if end_arg.is_nan() {
                            0
                        } else if end_arg < 0.0 {
                            (len + end_arg).max(0.0) as usize
                        } else {
                            end_arg.min(len) as usize
                        };

                        let new_ab = ab.slice(start, end).ok_or("Failed to slice ArrayBuffer")?;
                        Ok(Value::array_buffer(GcRef::new(new_ab)))
                    },
                    mm.clone(),
                    fn_proto,
                ),
            );
        }

        // Initialize Proxy constructor with static methods
        if name == "Proxy" {
            if let Some(ctor_obj) = ctor.as_object() {
                crate::intrinsics_impl::proxy::init_proxy_constructor(ctor_obj, fn_proto, &mm);
            }
        }

        // Per ES2023 §18: Global constructors are { writable: true, enumerable: false, configurable: true }
        global.define_property(
            PropertyKey::string(name),
            PropertyDescriptor::builtin_method(ctor),
        );
    }
}

// =============================================================================
// Global function implementations
// =============================================================================

/// Get argument at index, or undefined if missing
#[inline]
fn get_arg(args: &[Value], index: usize) -> Value {
    args.get(index).cloned().unwrap_or_default()
}

/// `eval(x)` - Evaluates JavaScript code represented as a string.
///
/// Currently, indirect eval is not fully supported. When called with a string,
/// it returns an error. This is a limitation to be addressed in a future update.
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
    _ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let encoded = to_string(&input);

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
    _ncx: &mut crate::context::NativeContext<'_>,
) -> Result<Value, VmError> {
    let input = get_arg(args, 0);
    let encoded = to_string(&input);

    decode_uri_impl(&encoded, false)
}

/// Common implementation for decodeURI and decodeURIComponent
fn decode_uri_impl(encoded: &str, preserve_reserved: bool) -> Result<Value, VmError> {
    let mut result = Vec::with_capacity(encoded.len());
    let mut chars = encoded.chars().peekable();

    while let Some(c) = chars.next() {
        if c == '%' {
            // Collect hex digits
            let mut hex_chars = String::with_capacity(2);
            for _ in 0..2 {
                match chars.next() {
                    Some(h) if h.is_ascii_hexdigit() => hex_chars.push(h),
                    _ => return Err(VmError::type_error("URIError: malformed URI sequence")),
                }
            }

            let byte = u8::from_str_radix(&hex_chars, 16)
                .map_err(|_| "URIError: malformed URI sequence".to_string())?;

            // For decodeURI, check if this is a reserved character
            if preserve_reserved && URI_RESERVED.contains(byte as char) && byte < 128 {
                // Keep the encoded form
                result.push(b'%');
                for b in hex_chars.bytes() {
                    result.push(b);
                }
            } else {
                result.push(byte);
            }
        } else {
            // Regular character: encode as UTF-8
            let mut buf = [0u8; 4];
            let encoded_char = c.encode_utf8(&mut buf);
            result.extend_from_slice(encoded_char.as_bytes());
        }
    }

    // Convert bytes to string
    let decoded =
        String::from_utf8(result).map_err(|_| "URIError: malformed URI sequence".to_string())?;

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
    if let Some(crate::value::HeapRef::BigInt(b)) = value.heap_ref() {
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

    // Integer check: if no fractional part AND magnitude < 10^21
    if abs_n.fract() == 0.0 && abs_n < 1e21 {
        // For values that fit in u64, use integer formatting directly
        if abs_n <= u64::MAX as f64 {
            let int_val = abs_n as u64;
            return if negative {
                format!("-{}", int_val)
            } else {
                format!("{}", int_val)
            };
        }
        // For large integers (> u64::MAX but < 1e21), format as fixed-point
        // and strip the trailing ".0" that Rust adds
        let s = format!("{:.0}", abs_n);
        return if negative { format!("-{}", s) } else { s };
    }

    // For all other numbers, use shortest representation matching JS semantics.
    // Rust's {:e} gives scientific notation; we reformat to match JS output.
    //
    // Strategy: get the significant digits via ryu-like formatting, then
    // apply JS exponent rules.
    //
    // JS rules for non-integer or large numbers:
    // - If 1 significant digit: "Ne+X" format
    // - If multiple significant digits: "N.DDDe+X" format
    // - Small exponents (0..20): use plain decimal notation
    // - Negative exponents (-6..0): use "0.000...N" notation — NO, JS uses plain for these too up to a point
    //
    // Actually: JS uses plain notation when the number can be written without
    // too many zeros. The exact rule from the spec (7.1.12.1):
    // - Let n, k, s be such that s × 10^(n-k) = abs(value), k is minimal
    // - If k ≤ n ≤ 21: output digits + (n-k) zeros (integer-like)
    // - If 0 < n ≤ 0 (impossible) ...
    // - If 0 < n ≤ k: digits with decimal point after n digits (e.g. "1.5")
    // - If -6 < n ≤ 0: "0." + |n| zeros + digits (e.g. "0.001")
    // - Otherwise: scientific notation

    // Get shortest decimal representation
    let repr = format!("{:e}", abs_n);
    // Parse mantissa and exponent from Rust's scientific notation
    let (mantissa_str, exp) = if let Some(pos) = repr.find('e') {
        let m = &repr[..pos];
        let e: i32 = repr[pos + 1..].parse().unwrap_or(0);
        (m.to_string(), e)
    } else {
        (repr.clone(), 0)
    };

    // Extract significant digits (remove the decimal point from mantissa)
    let digits: String = mantissa_str.chars().filter(|c| *c != '.').collect();
    let k = digits.len() as i32; // number of significant digits
    // n = exponent of most significant digit + 1
    // In Rust's {:e}, "1.23e5" means 1.23 × 10^5, so n = exp + 1
    let n = exp + 1;

    let result = if k <= n && n <= 21 {
        // Case: integer-like, append zeros
        let mut s = digits.clone();
        for _ in 0..(n - k) {
            s.push('0');
        }
        s
    } else if 0 < n && n <= k {
        // Case: decimal point within the digits
        let mut s = String::new();
        s.push_str(&digits[..n as usize]);
        s.push('.');
        s.push_str(&digits[n as usize..]);
        // Trim trailing zeros after decimal
        while s.ends_with('0') {
            s.pop();
        }
        if s.ends_with('.') {
            s.pop();
        }
        s
    } else if -6 < n && n <= 0 {
        // Case: "0.000...digits"
        let mut s = String::from("0.");
        for _ in 0..(-n) {
            s.push('0');
        }
        s.push_str(&digits);
        // Trim trailing zeros
        while s.ends_with('0') {
            s.pop();
        }
        s
    } else {
        // Scientific notation
        if k == 1 {
            format!(
                "{}e{}{}",
                &digits[..1],
                if n - 1 >= 0 { "+" } else { "" },
                n - 1
            )
        } else {
            let mut sig = String::new();
            sig.push_str(&digits[..1]);
            sig.push('.');
            sig.push_str(&digits[1..]);
            // Trim trailing zeros in significand
            while sig.ends_with('0') {
                sig.pop();
            }
            if sig.ends_with('.') {
                sig.pop();
            }
            format!("{}e{}{}", sig, if n - 1 >= 0 { "+" } else { "" }, n - 1)
        }
    };

    if negative {
        format!("-{}", result)
    } else {
        result
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

    fn call_global(runtime: &crate::runtime::VmRuntime, fn_impl: GlobalFn, args: &[Value]) -> Result<Value, VmError> {
        let mut ctx = runtime.create_context();
        let interpreter = crate::interpreter::Interpreter::new();
        let mut ncx = crate::context::NativeContext::new(&mut ctx, &interpreter);
        fn_impl(&Value::undefined(), args, &mut ncx)
    }

    #[test]
    fn test_global_this_setup() {
        let runtime = crate::runtime::VmRuntime::new();
        let memory_manager = runtime.memory_manager().clone();
        let global = GcRef::new(JsObject::new(Value::null(), memory_manager.clone()));
        let fn_proto = GcRef::new(JsObject::new(Value::null(), memory_manager));
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
            call_global(&_rt, global_parse_int, &[Value::string(JsString::intern("42"))])
                .unwrap()
                .as_number(),
            Some(42.0)
        );
        assert_eq!(
            call_global(&_rt, global_parse_int, &[Value::string(JsString::intern("-123"))])
                .unwrap()
                .as_number(),
            Some(-123.0)
        );
        assert_eq!(
            call_global(&_rt, global_parse_int, &[Value::string(JsString::intern("+456"))])
                .unwrap()
                .as_number(),
            Some(456.0)
        );

        // With radix
        assert_eq!(
            call_global(&_rt,
                global_parse_int,
                &[Value::string(JsString::intern("ff")), Value::number(16.0)],
            )
            .unwrap()
            .as_number(),
            Some(255.0)
        );
        assert_eq!(
            call_global(&_rt,
                global_parse_int,
                &[Value::string(JsString::intern("1010")), Value::number(2.0)],
            )
            .unwrap()
            .as_number(),
            Some(10.0)
        );

        // Hex prefix
        assert_eq!(
            call_global(&_rt, global_parse_int, &[Value::string(JsString::intern("0xFF"))])
                .unwrap()
                .as_number(),
            Some(255.0)
        );

        // Stops at invalid char
        assert_eq!(
            call_global(&_rt,
                global_parse_int,
                &[Value::string(JsString::intern("123abc"))]
            )
            .unwrap()
            .as_number(),
            Some(123.0)
        );

        // Invalid - returns NaN
        let result = call_global(&_rt,
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
            call_global(&_rt,
                global_parse_float,
                &[Value::string(JsString::intern("3.5"))]
            )
            .unwrap()
            .as_number(),
            Some(3.5)
        );
        assert_eq!(
            call_global(&_rt,
                global_parse_float,
                &[Value::string(JsString::intern("-2.5"))]
            )
            .unwrap()
            .as_number(),
            Some(-2.5)
        );
        assert_eq!(
            call_global(&_rt,
                global_parse_float,
                &[Value::string(JsString::intern("  42  "))]
            )
            .unwrap()
            .as_number(),
            Some(42.0)
        );
        assert_eq!(
            call_global(&_rt,
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
        let result = call_global(&_rt,
            global_encode_uri_component,
            &[Value::string(JsString::intern("hello world"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");

        let result = call_global(&_rt,
            global_encode_uri_component,
            &[Value::string(JsString::intern("a=1&b=2"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "a%3D1%26b%3D2");
    }

    #[test]
    fn test_decode_uri_component() {
        let _rt = crate::runtime::VmRuntime::new();
        let result = call_global(&_rt,
            global_decode_uri_component,
            &[Value::string(JsString::intern("hello%20world"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello world");

        let result = call_global(&_rt,
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
        let result = call_global(&_rt,
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
        let result = call_global(&_rt,
            global_encode_uri,
            &[Value::string(JsString::intern("hello world"))],
        )
        .unwrap();
        assert_eq!(result.as_string().unwrap().as_str(), "hello%20world");
    }

    #[test]
    fn test_decode_uri() {
        let _rt = crate::runtime::VmRuntime::new();
        let result = call_global(&_rt,
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
        let result = call_global(&_rt, global_eval, &[Value::string(JsString::intern("1 + 1"))]);
        assert!(result.is_err());
    }
}
