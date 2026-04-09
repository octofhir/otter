//! DataView constructor and prototype intrinsics.
//!
//! Spec references:
//! - DataView Objects: <https://tc39.es/ecma262/#sec-dataview-objects>
//! - DataView Constructor: <https://tc39.es/ecma262/#sec-dataview-constructor>
//! - DataView.prototype: <https://tc39.es/ecma262/#sec-properties-of-the-dataview-prototype-object>
//! - GetViewValue: <https://tc39.es/ecma262/#sec-getviewvalue>
//! - SetViewValue: <https://tc39.es/ecma262/#sec-setviewvalue>

use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static DATA_VIEW_INTRINSIC: DataViewIntrinsic = DataViewIntrinsic;

pub(super) struct DataViewIntrinsic;

impl IntrinsicInstaller for DataViewIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = data_view_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("DataView class descriptor should normalize")
            .build();

        if let Some(ctor_desc) = plan.constructor() {
            let host_id = cx.native_functions.register(ctor_desc.clone());
            intrinsics.data_view_constructor =
                cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype())?;
        }

        install_class_plan(
            intrinsics.data_view_prototype,
            intrinsics.data_view_constructor,
            &plan,
            intrinsics.function_prototype,
            cx,
        )?;

        // §25.3.4.1 get DataView.prototype.buffer
        install_getter(
            intrinsics.data_view_prototype,
            "buffer",
            data_view_get_buffer,
            intrinsics,
            cx,
        )?;
        // §25.3.4.2 get DataView.prototype.byteLength
        install_getter(
            intrinsics.data_view_prototype,
            "byteLength",
            data_view_get_byte_length,
            intrinsics,
            cx,
        )?;
        // §25.3.4.3 get DataView.prototype.byteOffset
        install_getter(
            intrinsics.data_view_prototype,
            "byteOffset",
            data_view_get_byte_offset,
            intrinsics,
            cx,
        )?;

        // §25.3.4 @@toStringTag
        let tag_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag_str = cx.heap.alloc_string("DataView");
        cx.heap.define_own_property(
            intrinsics.data_view_prototype,
            tag_symbol,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(tag_str.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "DataView",
            RegisterValue::from_object_handle(intrinsics.data_view_constructor.0),
        )
    }
}

fn proto(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Prototype,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn data_view_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("DataView")
        .with_constructor(
            NativeFunctionDescriptor::constructor("DataView", 1, data_view_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::DataViewPrototype),
        )
        // §25.3.4.5–16 getXxx methods
        .with_binding(proto("getInt8", 1, data_view_get_int8))
        .with_binding(proto("getUint8", 1, data_view_get_uint8))
        .with_binding(proto("getInt16", 1, data_view_get_int16))
        .with_binding(proto("getUint16", 1, data_view_get_uint16))
        .with_binding(proto("getInt32", 1, data_view_get_int32))
        .with_binding(proto("getUint32", 1, data_view_get_uint32))
        .with_binding(proto("getFloat32", 1, data_view_get_float32))
        .with_binding(proto("getFloat64", 1, data_view_get_float64))
        .with_binding(proto("getBigInt64", 1, data_view_get_big_int64))
        .with_binding(proto("getBigUint64", 1, data_view_get_big_uint64))
        // §25.3.4.17–28 setXxx methods
        .with_binding(proto("setInt8", 2, data_view_set_int8))
        .with_binding(proto("setUint8", 2, data_view_set_uint8))
        .with_binding(proto("setInt16", 2, data_view_set_int16))
        .with_binding(proto("setUint16", 2, data_view_set_uint16))
        .with_binding(proto("setInt32", 2, data_view_set_int32))
        .with_binding(proto("setUint32", 2, data_view_set_uint32))
        .with_binding(proto("setFloat32", 2, data_view_set_float32))
        .with_binding(proto("setFloat64", 2, data_view_set_float64))
        .with_binding(proto("setBigInt64", 2, data_view_set_big_int64))
        .with_binding(proto("setBigUint64", 2, data_view_set_big_uint64))
}

fn install_getter(
    target: ObjectHandle,
    name: &str,
    callback: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let getter_desc = NativeFunctionDescriptor::getter(name, callback);
    let getter_id = cx.native_functions.register(getter_desc);
    let getter_handle =
        cx.alloc_intrinsic_host_function(getter_id, intrinsics.function_prototype())?;
    let property = cx.property_names.intern(name);
    cx.heap
        .define_accessor(target, property, Some(getter_handle), None)?;
    Ok(())
}

// ── Helpers ──────────────────────────────────────────────────────────

fn type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<VmNativeCallError, VmNativeCallError> {
    let error = runtime.alloc_type_error(message).map_err(|error| {
        VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
    })?;
    Ok(VmNativeCallError::Thrown(
        RegisterValue::from_object_handle(error.0),
    ))
}

fn range_error(runtime: &mut crate::interpreter::RuntimeState, message: &str) -> VmNativeCallError {
    let prototype = runtime.intrinsics().range_error_prototype;
    let handle = runtime.alloc_object_with_prototype(Some(prototype));
    let msg = runtime.alloc_string(message);
    let msg_prop = runtime.intern_property_name("message");
    runtime
        .objects_mut()
        .set_property(handle, msg_prop, RegisterValue::from_object_handle(msg.0))
        .ok();
    VmNativeCallError::Thrown(RegisterValue::from_object_handle(handle.0))
}

/// §7.1.22 ToIndex
/// <https://tc39.es/ecma262/#sec-toindex>
fn to_index(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<usize, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok(0);
    }
    let number = runtime
        .js_to_number(value)
        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
    let integer_index = if number.is_nan() || number == 0.0 {
        0.0
    } else if number.is_infinite() {
        number
    } else {
        number.trunc()
    };
    if integer_index < 0.0 {
        return Err(range_error(runtime, "Invalid DataView offset"));
    }
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0;
    let index = if integer_index.is_infinite() {
        MAX_SAFE
    } else {
        integer_index.min(MAX_SAFE)
    };
    if integer_index != index {
        return Err(range_error(runtime, "Invalid DataView offset"));
    }
    Ok(index as usize)
}

fn require_data_view_this(
    this: &RegisterValue,
    method_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not a DataView"),
        )?);
    };
    if !matches!(runtime.objects().kind(handle), Ok(HeapValueKind::DataView)) {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not a DataView"),
        )?);
    }
    Ok(handle)
}

/// Check that the viewed buffer is not detached.
fn require_not_detached(
    handle: ObjectHandle,
    method_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let buf = runtime
        .objects()
        .data_view_buffer(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    let kind = runtime.objects().kind(buf);
    if let Ok(HeapValueKind::ArrayBuffer) = kind
        && runtime
            .objects()
            .array_buffer_is_detached(buf)
            .unwrap_or(false)
    {
        return Err(type_error(
            runtime,
            &format!("{method_name}: viewed ArrayBuffer is detached"),
        )?);
    }
    Ok(())
}

// ── Constructor ──────────────────────────────────────────────────────

/// §25.3.2.1 DataView ( buffer, byteOffset, byteLength )
/// <https://tc39.es/ecma262/#sec-dataview-constructor>
fn data_view_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. If NewTarget is undefined, throw a TypeError.
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(runtime, "Constructor DataView requires 'new'")?);
    }

    // 2. buffer must be an ArrayBuffer or SharedArrayBuffer.
    let buffer_val = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(buffer_raw) = buffer_val.as_object_handle() else {
        return Err(type_error(
            runtime,
            "DataView: first argument must be an ArrayBuffer or SharedArrayBuffer",
        )?);
    };
    let buffer = ObjectHandle(buffer_raw);
    let buf_kind = runtime
        .objects()
        .kind(buffer)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    if !matches!(
        buf_kind,
        HeapValueKind::ArrayBuffer | HeapValueKind::SharedArrayBuffer
    ) {
        return Err(type_error(
            runtime,
            "DataView: first argument must be an ArrayBuffer or SharedArrayBuffer",
        )?);
    }

    // 3. Let offset be ? ToIndex(byteOffset).
    let offset = to_index(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // 4. If IsDetachedBuffer(buffer), throw TypeError.
    if buf_kind == HeapValueKind::ArrayBuffer
        && runtime
            .objects()
            .array_buffer_is_detached(buffer)
            .unwrap_or(false)
    {
        return Err(type_error(runtime, "DataView: buffer is detached")?);
    }

    // 5. Let bufferByteLength be buffer.[[ArrayBufferByteLength]].
    let buffer_byte_length = match buf_kind {
        HeapValueKind::ArrayBuffer => runtime
            .objects()
            .array_buffer_byte_length(buffer)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?,
        HeapValueKind::SharedArrayBuffer => runtime
            .objects()
            .shared_array_buffer_byte_length(buffer)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?,
        _ => unreachable!(),
    };

    // 6. If offset > bufferByteLength, throw RangeError.
    if offset > buffer_byte_length {
        return Err(range_error(
            runtime,
            "DataView: byteOffset exceeds buffer length",
        ));
    }

    // 7–8. Determine view byte length.
    let byte_length_arg = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let byte_length = if byte_length_arg == RegisterValue::undefined() {
        // AUTO for resizable buffers, explicit for fixed-length.
        let is_resizable = if buf_kind == HeapValueKind::ArrayBuffer {
            runtime
                .objects()
                .array_buffer_is_resizable(buffer)
                .unwrap_or(false)
        } else {
            runtime
                .objects()
                .shared_array_buffer_is_growable(buffer)
                .unwrap_or(false)
        };
        if is_resizable {
            None // AUTO
        } else {
            Some(buffer_byte_length - offset)
        }
    } else {
        let view_byte_length = to_index(byte_length_arg, runtime)?;
        if offset + view_byte_length > buffer_byte_length {
            return Err(range_error(
                runtime,
                "DataView: byteOffset + byteLength exceeds buffer length",
            ));
        }
        Some(view_byte_length)
    };

    // §10.1.13 OrdinaryCreateFromConstructor — honour `newTarget.prototype`.
    let prototype = Some(
        runtime.subclass_prototype_or_default(*this, runtime.intrinsics().data_view_prototype),
    );
    let handle = runtime
        .objects_mut()
        .alloc_data_view(buffer, offset, byte_length, prototype);
    Ok(RegisterValue::from_object_handle(handle.0))
}

// ── Accessors ────────────────────────────────────────────────────────

/// §25.3.4.1 get DataView.prototype.buffer
/// <https://tc39.es/ecma262/#sec-get-dataview.prototype.buffer>
fn data_view_get_buffer(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_data_view_this(this, "get DataView.prototype.buffer", runtime)?;
    let buf = runtime
        .objects()
        .data_view_buffer(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_object_handle(buf.0))
}

/// §25.3.4.2 get DataView.prototype.byteLength
/// <https://tc39.es/ecma262/#sec-get-dataview.prototype.bytelength>
fn data_view_get_byte_length(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_data_view_this(this, "get DataView.prototype.byteLength", runtime)?;
    require_not_detached(handle, "get DataView.prototype.byteLength", runtime)?;
    let len = runtime
        .objects()
        .data_view_byte_length(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_number(len as f64))
}

/// §25.3.4.3 get DataView.prototype.byteOffset
/// <https://tc39.es/ecma262/#sec-get-dataview.prototype.byteoffset>
fn data_view_get_byte_offset(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_data_view_this(this, "get DataView.prototype.byteOffset", runtime)?;
    require_not_detached(handle, "get DataView.prototype.byteOffset", runtime)?;
    let offset = runtime
        .objects()
        .data_view_byte_offset(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    Ok(RegisterValue::from_number(offset as f64))
}

// ── GetViewValue / SetViewValue ──────────────────────────────────────

/// §25.3.1.1 GetViewValue ( view, requestIndex, isLittleEndian, type )
/// <https://tc39.es/ecma262/#sec-getviewvalue>
fn get_view_value(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
    element_size: usize,
    method_name: &str,
) -> Result<Vec<u8>, VmNativeCallError> {
    let handle = require_data_view_this(this, method_name, runtime)?;

    // 2. Let getIndex be ? ToIndex(requestIndex).
    let get_index = to_index(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // Check detached.
    require_not_detached(handle, method_name, runtime)?;

    // Read bytes.
    let bytes = runtime
        .objects()
        .data_view_get_bytes(handle, get_index, element_size)
        .map_err(|e| match e {
            crate::object::ObjectError::InvalidArrayLength => {
                range_error(runtime, &format!("{method_name}: index out of range"))
            }
            other => VmNativeCallError::Internal(format!("{other:?}").into()),
        })?;
    Ok(bytes)
}

/// §25.3.1.2 SetViewValue ( view, requestIndex, isLittleEndian, type, value )
/// <https://tc39.es/ecma262/#sec-setviewvalue>
fn set_view_value(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
    element_size: usize,
    bytes: &[u8],
    method_name: &str,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_data_view_this(this, method_name, runtime)?;

    // 2. Let getIndex be ? ToIndex(requestIndex).
    let set_index = to_index(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // Re-check detached (ToNumber may have detached).
    require_not_detached(handle, method_name, runtime)?;

    let _ = element_size; // validated by caller via bytes.len()
    runtime
        .objects_mut()
        .data_view_set_bytes(handle, set_index, bytes)
        .map_err(|e| match e {
            crate::object::ObjectError::InvalidArrayLength => {
                range_error(runtime, &format!("{method_name}: index out of range"))
            }
            other => VmNativeCallError::Internal(format!("{other:?}").into()),
        })?;
    Ok(RegisterValue::undefined())
}

/// Determine endianness from the littleEndian argument.
fn is_little_endian(args: &[RegisterValue], offset: usize) -> bool {
    args.get(offset).map(|v| v.is_truthy()).unwrap_or(false)
}

// ── Spec-correct numeric conversions ─────────────────────────────────
// Rust `as` casts saturate; the ES spec uses modular arithmetic.
// §7.1.10 ToInt8, §7.1.11 ToUint8, §7.1.8 ToInt16, §7.1.9 ToUint16,
// §7.1.6 ToInt32, §7.1.7 ToUint32
// <https://tc39.es/ecma262/#sec-toint8>

/// Convert f64 to integer then truncate to u32 per spec (modular 2^32).
fn to_uint32(n: f64) -> u32 {
    if n.is_nan() || n.is_infinite() || n == 0.0 {
        return 0;
    }
    // §7.1.7: sign(n) * floor(abs(n)) modulo 2^32
    let i = n.trunc();
    (i as i64) as u32
}

fn to_int8(n: f64) -> i8 {
    to_uint32(n) as u8 as i8
}

fn to_uint8(n: f64) -> u8 {
    to_uint32(n) as u8
}

fn to_int16(n: f64) -> i16 {
    to_uint32(n) as u16 as i16
}

fn to_uint16(n: f64) -> u16 {
    to_uint32(n) as u16
}

fn to_int32(n: f64) -> i32 {
    to_uint32(n) as i32
}

// ── Get methods ──────────────────────────────────────────────────────

/// §25.3.4.5 DataView.prototype.getInt8 ( byteOffset )
fn data_view_get_int8(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 1, "DataView.prototype.getInt8")?;
    Ok(RegisterValue::from_i32(bytes[0] as i8 as i32))
}

/// §25.3.4.6 DataView.prototype.getUint8 ( byteOffset )
fn data_view_get_uint8(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 1, "DataView.prototype.getUint8")?;
    Ok(RegisterValue::from_i32(bytes[0] as i32))
}

/// §25.3.4.9 DataView.prototype.getInt16 ( byteOffset [ , littleEndian ] )
fn data_view_get_int16(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 2, "DataView.prototype.getInt16")?;
    let val = if is_little_endian(args, 1) {
        i16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        i16::from_be_bytes([bytes[0], bytes[1]])
    };
    Ok(RegisterValue::from_i32(val as i32))
}

/// §25.3.4.11 DataView.prototype.getUint16 ( byteOffset [ , littleEndian ] )
fn data_view_get_uint16(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 2, "DataView.prototype.getUint16")?;
    let val = if is_little_endian(args, 1) {
        u16::from_le_bytes([bytes[0], bytes[1]])
    } else {
        u16::from_be_bytes([bytes[0], bytes[1]])
    };
    Ok(RegisterValue::from_i32(val as i32))
}

/// §25.3.4.8 DataView.prototype.getInt32 ( byteOffset [ , littleEndian ] )
fn data_view_get_int32(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 4, "DataView.prototype.getInt32")?;
    let val = if is_little_endian(args, 1) {
        i32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        i32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    };
    Ok(RegisterValue::from_i32(val))
}

/// §25.3.4.12 DataView.prototype.getUint32 ( byteOffset [ , littleEndian ] )
fn data_view_get_uint32(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 4, "DataView.prototype.getUint32")?;
    let val = if is_little_endian(args, 1) {
        u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        u32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    };
    Ok(RegisterValue::from_number(val as f64))
}

/// §25.3.4.7 DataView.prototype.getFloat32 ( byteOffset [ , littleEndian ] )
fn data_view_get_float32(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 4, "DataView.prototype.getFloat32")?;
    let val = if is_little_endian(args, 1) {
        f32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    } else {
        f32::from_be_bytes([bytes[0], bytes[1], bytes[2], bytes[3]])
    };
    Ok(RegisterValue::from_number(val as f64))
}

/// §25.3.4.6 DataView.prototype.getFloat64 ( byteOffset [ , littleEndian ] )
fn data_view_get_float64(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 8, "DataView.prototype.getFloat64")?;
    let val = if is_little_endian(args, 1) {
        f64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    } else {
        f64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    };
    Ok(RegisterValue::from_number(val))
}

/// §25.3.4.4 DataView.prototype.getBigInt64 ( byteOffset [ , littleEndian ] )
/// <https://tc39.es/ecma262/#sec-dataview.prototype.getbigint64>
fn data_view_get_big_int64(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 8, "DataView.prototype.getBigInt64")?;
    let val = if is_little_endian(args, 1) {
        i64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    } else {
        i64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    };
    let handle = runtime.alloc_bigint(&val.to_string());
    Ok(RegisterValue::from_bigint_handle(handle.0))
}

/// §25.3.4.5 DataView.prototype.getBigUint64 ( byteOffset [ , littleEndian ] )
/// <https://tc39.es/ecma262/#sec-dataview.prototype.getbiguint64>
fn data_view_get_big_uint64(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let bytes = get_view_value(this, args, runtime, 8, "DataView.prototype.getBigUint64")?;
    let val = if is_little_endian(args, 1) {
        u64::from_le_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    } else {
        u64::from_be_bytes([
            bytes[0], bytes[1], bytes[2], bytes[3], bytes[4], bytes[5], bytes[6], bytes[7],
        ])
    };
    let handle = runtime.alloc_bigint(&val.to_string());
    Ok(RegisterValue::from_bigint_handle(handle.0))
}

// ── Set methods ──────────────────────────────────────────────────────

/// §25.3.4.17 DataView.prototype.setInt8 ( byteOffset, value )
fn data_view_set_int8(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let bytes = [to_int8(num) as u8];
    set_view_value(this, args, runtime, 1, &bytes, "DataView.prototype.setInt8")
}

/// §25.3.4.18 DataView.prototype.setUint8 ( byteOffset, value )
fn data_view_set_uint8(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let bytes = [to_uint8(num)];
    set_view_value(
        this,
        args,
        runtime,
        1,
        &bytes,
        "DataView.prototype.setUint8",
    )
}

/// §25.3.4.19 DataView.prototype.setInt16 ( byteOffset, value [ , littleEndian ] )
fn data_view_set_int16(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let val = to_int16(num);
    let bytes = if is_little_endian(args, 2) {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        2,
        &bytes,
        "DataView.prototype.setInt16",
    )
}

/// §25.3.4.21 DataView.prototype.setUint16 ( byteOffset, value [ , littleEndian ] )
fn data_view_set_uint16(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let val = to_uint16(num);
    let bytes = if is_little_endian(args, 2) {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        2,
        &bytes,
        "DataView.prototype.setUint16",
    )
}

/// §25.3.4.20 DataView.prototype.setInt32 ( byteOffset, value [ , littleEndian ] )
fn data_view_set_int32(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let val = to_int32(num);
    let bytes = if is_little_endian(args, 2) {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        4,
        &bytes,
        "DataView.prototype.setInt32",
    )
}

/// §25.3.4.22 DataView.prototype.setUint32 ( byteOffset, value [ , littleEndian ] )
fn data_view_set_uint32(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let val = to_uint32(num);
    let bytes = if is_little_endian(args, 2) {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        4,
        &bytes,
        "DataView.prototype.setUint32",
    )
}

/// §25.3.4.23 DataView.prototype.setFloat32 ( byteOffset, value [ , littleEndian ] )
fn data_view_set_float32(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let val = num as f32;
    let bytes = if is_little_endian(args, 2) {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        4,
        &bytes,
        "DataView.prototype.setFloat32",
    )
}

/// §25.3.4.24 DataView.prototype.setFloat64 ( byteOffset, value [ , littleEndian ] )
fn data_view_set_float64(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let num = to_number_arg(args, 1, runtime)?;
    let bytes = if is_little_endian(args, 2) {
        num.to_le_bytes()
    } else {
        num.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        8,
        &bytes,
        "DataView.prototype.setFloat64",
    )
}

/// §25.3.4.25 DataView.prototype.setBigInt64 ( byteOffset, value [ , littleEndian ] )
/// <https://tc39.es/ecma262/#sec-dataview.prototype.setbigint64>
fn data_view_set_big_int64(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = to_bigint_i64(args, 1, runtime)?;
    let bytes = if is_little_endian(args, 2) {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        8,
        &bytes,
        "DataView.prototype.setBigInt64",
    )
}

/// §25.3.4.26 DataView.prototype.setBigUint64 ( byteOffset, value [ , littleEndian ] )
/// <https://tc39.es/ecma262/#sec-dataview.prototype.setbiguint64>
fn data_view_set_big_uint64(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let val = to_bigint_u64(args, 1, runtime)?;
    let bytes = if is_little_endian(args, 2) {
        val.to_le_bytes()
    } else {
        val.to_be_bytes()
    };
    set_view_value(
        this,
        args,
        runtime,
        8,
        &bytes,
        "DataView.prototype.setBigUint64",
    )
}

/// Helper: extract BigInt argument at given index and convert to `i64`.
///
/// §7.1.13 ToBigInt — applied to the value argument for setBigInt64.
fn to_bigint_i64(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<i64, VmNativeCallError> {
    let val = args
        .get(index)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = val.as_bigint_handle() else {
        return Err(type_error(
            runtime,
            "Cannot convert a non-BigInt value to a BigInt",
        )?);
    };
    let s = runtime
        .bigint_value(ObjectHandle(handle))
        .ok_or_else(|| VmNativeCallError::Internal("invalid BigInt handle".into()))?;
    Ok(s.parse::<i64>().unwrap_or(0))
}

/// Helper: extract BigInt argument at given index and convert to `u64`.
///
/// §7.1.13 ToBigInt — applied to the value argument for setBigUint64.
fn to_bigint_u64(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<u64, VmNativeCallError> {
    let val = args
        .get(index)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = val.as_bigint_handle() else {
        return Err(type_error(
            runtime,
            "Cannot convert a non-BigInt value to a BigInt",
        )?);
    };
    let s = runtime
        .bigint_value(ObjectHandle(handle))
        .ok_or_else(|| VmNativeCallError::Internal("invalid BigInt handle".into()))?;
    // For BigUint64, parse as i128 first to handle large positive values and wrap.
    let parsed: i128 = s.parse().unwrap_or(0);
    Ok(parsed as u64)
}

/// Helper: extract numeric argument at given index.
fn to_number_arg(
    args: &[RegisterValue],
    index: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    let val = args
        .get(index)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .js_to_number(val)
        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))
}
