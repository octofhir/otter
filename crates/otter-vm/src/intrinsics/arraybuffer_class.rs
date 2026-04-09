//! ArrayBuffer constructor and prototype intrinsics.
//!
//! Spec references:
//! - ArrayBuffer Objects: <https://tc39.es/ecma262/#sec-arraybuffer-objects>
//! - ArrayBuffer Constructor: <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
//! - get ArrayBuffer.prototype.byteLength:
//!   <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.bytelength>
//! - ArrayBuffer.prototype.slice:
//!   <https://tc39.es/ecma262/#sec-arraybuffer.prototype.slice>

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

pub(super) static ARRAY_BUFFER_INTRINSIC: ArrayBufferIntrinsic = ArrayBufferIntrinsic;

pub(super) struct ArrayBufferIntrinsic;

impl IntrinsicInstaller for ArrayBufferIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = array_buffer_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("ArrayBuffer class descriptor should normalize")
            .build();

        if let Some(ctor_desc) = plan.constructor() {
            let host_id = cx.native_functions.register(ctor_desc.clone());
            intrinsics.array_buffer_constructor =
                cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype())?;
        }

        install_class_plan(
            intrinsics.array_buffer_prototype,
            intrinsics.array_buffer_constructor,
            &plan,
            intrinsics.function_prototype,
            cx,
        )?;

        install_getter(
            intrinsics.array_buffer_prototype,
            "byteLength",
            array_buffer_get_byte_length,
            intrinsics,
            cx,
        )?;
        // §25.1.5.3 get ArrayBuffer.prototype.detached
        install_getter(
            intrinsics.array_buffer_prototype,
            "detached",
            array_buffer_get_detached,
            intrinsics,
            cx,
        )?;
        // §25.1.5.4 get ArrayBuffer.prototype.maxByteLength
        install_getter(
            intrinsics.array_buffer_prototype,
            "maxByteLength",
            array_buffer_get_max_byte_length,
            intrinsics,
            cx,
        )?;
        // §25.1.5.5 get ArrayBuffer.prototype.resizable
        install_getter(
            intrinsics.array_buffer_prototype,
            "resizable",
            array_buffer_get_resizable,
            intrinsics,
            cx,
        )?;

        let tag_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag_str = cx.heap.alloc_string("ArrayBuffer");
        cx.heap.define_own_property(
            intrinsics.array_buffer_prototype,
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
            "ArrayBuffer",
            RegisterValue::from_object_handle(intrinsics.array_buffer_constructor.0),
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

fn stat(
    name: &str,
    arity: u16,
    f: fn(
        &RegisterValue,
        &[RegisterValue],
        &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError>,
) -> NativeBindingDescriptor {
    NativeBindingDescriptor::new(
        NativeBindingTarget::Constructor,
        NativeFunctionDescriptor::method(name, arity, f),
    )
}

fn array_buffer_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("ArrayBuffer")
        .with_constructor(
            NativeFunctionDescriptor::constructor("ArrayBuffer", 1, array_buffer_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::ArrayBufferPrototype),
        )
        .with_binding(proto("slice", 2, array_buffer_slice))
        .with_binding(proto("resize", 1, array_buffer_resize))
        .with_binding(proto("transfer", 0, array_buffer_transfer))
        .with_binding(proto(
            "transferToFixedLength",
            0,
            array_buffer_transfer_to_fixed_length,
        ))
        .with_binding(stat("isView", 1, array_buffer_is_view))
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

/// §7.1.22 ToIndex ( value )
/// <https://tc39.es/ecma262/#sec-toindex>
fn to_index(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<usize, VmNativeCallError> {
    // 1. If value is undefined, return 0.
    if value == RegisterValue::undefined() {
        return Ok(0);
    }
    // 2. Let integerIndex be ? ToIntegerOrInfinity(value).
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
    // 3. If integerIndex < 0, throw a RangeError exception.
    if integer_index < 0.0 {
        return Err(range_error(runtime, "Invalid ArrayBuffer length"));
    }
    // 4. Let index be ! ToLength(integerIndex).  (clamps to 2^53 - 1)
    const MAX_SAFE: f64 = 9_007_199_254_740_991.0; // 2^53 - 1
    let index = if integer_index.is_infinite() {
        MAX_SAFE
    } else {
        integer_index.min(MAX_SAFE)
    };
    // 5. If SameValue(integerIndex, index) is false, throw a RangeError exception.
    if integer_index != index {
        return Err(range_error(runtime, "Invalid ArrayBuffer length"));
    }
    Ok(index as usize)
}

fn to_integer_or_infinity(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok(0.0);
    }
    let number = runtime
        .js_to_number(value)
        .map_err(|error| VmNativeCallError::Internal(format!("{error}").into()))?;
    if number.is_nan() {
        return Ok(0.0);
    }
    if number.is_infinite() {
        return Ok(number);
    }
    Ok(number.trunc())
}

fn require_array_buffer_this(
    this: &RegisterValue,
    method_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not an ArrayBuffer"),
        )?);
    };
    if !matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::ArrayBuffer)
    ) {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not an ArrayBuffer"),
        )?);
    }
    Ok(handle)
}

/// Extract `maxByteLength` from options object if present.
///
/// §25.1.3.1 step 5-6
/// <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
fn get_max_byte_length_option(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<usize>, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok(None);
    }
    let Some(options) = value.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "ArrayBuffer options must be an object",
        )?);
    };
    let property = runtime.intern_property_name("maxByteLength");
    let option = runtime.ordinary_get(options, property, value)?;
    if option == RegisterValue::undefined() {
        return Ok(None);
    }
    Ok(Some(to_index(option, runtime)?))
}

/// §25.1.3.1 ArrayBuffer ( length [, options] )
/// <https://tc39.es/ecma262/#sec-arraybuffer-constructor>
fn array_buffer_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // 1. If NewTarget is undefined, throw a TypeError exception.
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(
            runtime,
            "Constructor ArrayBuffer requires 'new'",
        )?);
    }

    // 2. Let byteLength be ? ToIndex(length).
    let byte_length = to_index(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // 3-6. Let requestedMaxByteLength be ? GetArrayBufferMaxByteLengthOption(options).
    let requested_max = get_max_byte_length_option(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // §10.1.13 OrdinaryCreateFromConstructor — honour `newTarget.prototype`.
    let prototype = Some(
        runtime.subclass_prototype_or_default(*this, runtime.intrinsics().array_buffer_prototype),
    );
    // §25.1.2.1 CreateByteDataBlock — surfaces an `OutOfMemory` ObjectError
    // when the requested byte budget exceeds the configured heap cap. We
    // map it to a catchable `RangeError` here so test262's
    // `new ArrayBuffer(2**52)` style tests behave per spec instead of
    // aborting the host process via a Rust allocator panic.
    let result = match requested_max {
        Some(max_byte_length) => {
            // 7. If requestedMaxByteLength is not empty:
            //    a. If byteLength > requestedMaxByteLength, throw a RangeError.
            if byte_length > max_byte_length {
                return Err(range_error(
                    runtime,
                    "ArrayBuffer byteLength exceeds maxByteLength",
                ));
            }
            runtime.objects_mut().alloc_array_buffer_resizable(
                byte_length,
                max_byte_length,
                prototype,
            )
        }
        None => {
            // 8. Else, AllocateArrayBuffer(NewTarget, byteLength).
            runtime
                .objects_mut()
                .alloc_array_buffer(byte_length, prototype)
        }
    };
    let handle = result.map_err(|err| match err {
        crate::object::ObjectError::OutOfMemory => range_error(
            runtime,
            "ArrayBuffer allocation failed: byte length exceeds heap limit",
        ),
        other => {
            VmNativeCallError::Internal(format!("ArrayBuffer allocation: {other:?}").into())
        }
    })?;
    Ok(RegisterValue::from_object_handle(handle.0))
}

// §25.1.5.1 get ArrayBuffer.prototype.byteLength
fn array_buffer_get_byte_length(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_array_buffer_this(this, "get ArrayBuffer.prototype.byteLength", runtime)?;
    let byte_length = runtime
        .objects()
        .array_buffer_byte_length(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(i32::try_from(byte_length)
        .map(RegisterValue::from_i32)
        .unwrap_or_else(|_| RegisterValue::from_number(byte_length as f64)))
}

/// §25.1.5.3 get ArrayBuffer.prototype.detached
/// <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.detached>
fn array_buffer_get_detached(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_array_buffer_this(this, "get ArrayBuffer.prototype.detached", runtime)?;
    let detached = runtime
        .objects()
        .array_buffer_is_detached(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_bool(detached))
}

/// §25.1.5.4 get ArrayBuffer.prototype.maxByteLength
/// <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.maxbytelength>
fn array_buffer_get_max_byte_length(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle =
        require_array_buffer_this(this, "get ArrayBuffer.prototype.maxByteLength", runtime)?;
    // 3. If IsDetachedBuffer(O) is true, throw a TypeError.
    if runtime
        .objects()
        .array_buffer_is_detached(handle)
        .unwrap_or(false)
    {
        return Err(type_error(
            runtime,
            "Cannot access maxByteLength of a detached ArrayBuffer",
        )?);
    }
    let max = runtime
        .objects()
        .array_buffer_max_byte_length(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_number(max as f64))
}

/// §25.1.5.5 get ArrayBuffer.prototype.resizable
/// <https://tc39.es/ecma262/#sec-get-arraybuffer.prototype.resizable>
fn array_buffer_get_resizable(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_array_buffer_this(this, "get ArrayBuffer.prototype.resizable", runtime)?;
    let resizable = runtime
        .objects()
        .array_buffer_is_resizable(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_bool(resizable))
}

/// §25.1.5.4 ArrayBuffer.prototype.slice ( start, end )
/// <https://tc39.es/ecma262/#sec-arraybuffer.prototype.slice>
fn array_buffer_slice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_array_buffer_this(this, "ArrayBuffer.prototype.slice", runtime)?;
    // 3. If IsDetachedBuffer(O) is true, throw a TypeError.
    if runtime
        .objects()
        .array_buffer_is_detached(handle)
        .unwrap_or(false)
    {
        return Err(type_error(runtime, "Cannot slice a detached ArrayBuffer")?);
    }
    let len = runtime
        .objects()
        .array_buffer_byte_length(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    let len_f64 = len as f64;

    let start = to_integer_or_infinity(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let end = if let Some(end) = args.get(1).copied() {
        if end == RegisterValue::undefined() {
            len_f64
        } else {
            to_integer_or_infinity(end, runtime)?
        }
    } else {
        len_f64
    };

    let first = if start.is_sign_negative() {
        (len_f64 + start).max(0.0)
    } else {
        start.min(len_f64)
    };
    let final_ = if end.is_sign_negative() {
        (len_f64 + end).max(0.0)
    } else {
        end.min(len_f64)
    };

    let first = if first.is_infinite() {
        0usize
    } else {
        first as usize
    };
    let final_ = if final_.is_infinite() {
        len
    } else {
        final_ as usize
    };
    let prototype = Some(runtime.intrinsics().array_buffer_prototype);
    let result = runtime
        .objects_mut()
        .array_buffer_slice(handle, first, final_, prototype)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_object_handle(result.0))
}

/// §25.1.5.6 ArrayBuffer.prototype.resize ( newLength )
/// <https://tc39.es/ecma262/#sec-arraybuffer.prototype.resize>
fn array_buffer_resize(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_array_buffer_this(this, "ArrayBuffer.prototype.resize", runtime)?;
    // 3. If IsDetachedBuffer(O) is true, throw a TypeError.
    if runtime
        .objects()
        .array_buffer_is_detached(handle)
        .unwrap_or(false)
    {
        return Err(type_error(runtime, "Cannot resize a detached ArrayBuffer")?);
    }
    // 4. If IsResizableArrayBuffer(O) is false, throw a TypeError.
    if !runtime
        .objects()
        .array_buffer_is_resizable(handle)
        .unwrap_or(false)
    {
        return Err(type_error(
            runtime,
            "ArrayBuffer.prototype.resize requires a resizable ArrayBuffer",
        )?);
    }
    // 5. Let newByteLength be ? ToIndex(newLength).
    let new_byte_length = to_index(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    // 6-7. Resize the buffer (throws RangeError if newByteLength > maxByteLength).
    runtime
        .objects_mut()
        .array_buffer_resize(handle, new_byte_length)
        .map_err(|error| match error {
            crate::object::ObjectError::InvalidArrayLength => {
                range_error(runtime, "ArrayBuffer resize exceeds maxByteLength")
            }
            other => VmNativeCallError::Internal(format!("{other:?}").into()),
        })?;
    Ok(RegisterValue::undefined())
}

/// §25.1.5.7 ArrayBuffer.prototype.transfer ( [ newLength ] )
/// <https://tc39.es/ecma262/#sec-arraybuffer.prototype.transfer>
fn array_buffer_transfer(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    array_buffer_transfer_impl(this, args, runtime, true)
}

/// §25.1.5.8 ArrayBuffer.prototype.transferToFixedLength ( [ newLength ] )
/// <https://tc39.es/ecma262/#sec-arraybuffer.prototype.transfertofixedlength>
fn array_buffer_transfer_to_fixed_length(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    array_buffer_transfer_impl(this, args, runtime, false)
}

/// §25.1.2.13 ArrayBufferCopyAndDetach ( arrayBuffer, newLength, preserveResizability )
/// <https://tc39.es/ecma262/#sec-arraybuffercopyanddetach>
///
/// Shared implementation for transfer and transferToFixedLength.
fn array_buffer_transfer_impl(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
    preserve_resizability: bool,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_array_buffer_this(this, "ArrayBuffer.prototype.transfer", runtime)?;
    // 2. If IsDetachedBuffer(O) is true, throw a TypeError.
    if runtime
        .objects()
        .array_buffer_is_detached(handle)
        .unwrap_or(false)
    {
        return Err(type_error(
            runtime,
            "Cannot transfer a detached ArrayBuffer",
        )?);
    }

    let old_length = runtime
        .objects()
        .array_buffer_byte_length(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;

    // 3. If newLength is undefined, let newByteLength be oldLength. Else ToIndex(newLength).
    let new_byte_length_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let new_byte_length = if new_byte_length_arg == RegisterValue::undefined() {
        old_length
    } else {
        to_index(new_byte_length_arg, runtime)?
    };

    // Detach the source buffer and take its data.
    let (mut old_data, old_max, old_resizable) = runtime
        .objects_mut()
        .array_buffer_detach_for_transfer(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;

    // Resize/copy the data to newByteLength.
    old_data.resize(new_byte_length, 0);

    let prototype = Some(runtime.intrinsics().array_buffer_prototype);
    let new_handle = if preserve_resizability && old_resizable {
        // transfer preserves resizability — maxByteLength = max(old_max, newByteLength).
        let new_max = old_max.max(new_byte_length);
        runtime
            .objects_mut()
            .alloc_array_buffer_full(old_data, new_max, true, prototype)
    } else {
        // transferToFixedLength or source was fixed-length — always fixed.
        runtime
            .objects_mut()
            .alloc_array_buffer_with_data(old_data, prototype)
    };

    Ok(RegisterValue::from_object_handle(new_handle.0))
}

/// §25.1.4.1 ArrayBuffer.isView ( arg )
/// <https://tc39.es/ecma262/#sec-arraybuffer.isview>
fn array_buffer_is_view(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    // 1. If arg is not an Object, return false.
    let Some(handle_raw) = arg.as_object_handle() else {
        return Ok(RegisterValue::from_bool(false));
    };
    let handle = ObjectHandle(handle_raw);
    // 2. If arg has a [[ViewedArrayBuffer]] internal slot, return true.
    // This means TypedArray or DataView.
    let kind = runtime.objects().kind(handle);
    match kind {
        Ok(HeapValueKind::TypedArray | HeapValueKind::DataView) => {
            Ok(RegisterValue::from_bool(true))
        }
        _ => Ok(RegisterValue::from_bool(false)),
    }
}
