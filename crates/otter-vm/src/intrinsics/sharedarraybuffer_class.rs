//! SharedArrayBuffer constructor and prototype intrinsics.
//!
//! Spec references:
//! - SharedArrayBuffer Objects: <https://tc39.es/ecma262/#sec-sharedarraybuffer-objects>
//! - SharedArrayBuffer Constructor:
//!   <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
//! - SharedArrayBuffer(length[, options]):
//!   <https://tc39.es/ecma262/#sec-sharedarraybuffer-constructor>
//! - get SharedArrayBuffer.prototype.byteLength:
//!   <https://tc39.es/ecma262/#sec-get-sharedarraybuffer.prototype.bytelength>
//! - SharedArrayBuffer.prototype.grow(newLength):
//!   <https://tc39.es/ecma262/#sec-sharedarraybuffer.prototype.grow>
//! - get SharedArrayBuffer.prototype.growable:
//!   <https://tc39.es/ecma262/#sec-get-sharedarraybuffer.prototype.growable>
//! - get SharedArrayBuffer.prototype.maxByteLength:
//!   <https://tc39.es/ecma262/#sec-get-sharedarraybuffer.prototype.maxbytelength>
//! - SharedArrayBuffer.prototype.slice(start, end):
//!   <https://tc39.es/ecma262/#sec-sharedarraybuffer.prototype.slice>

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

pub(super) static SHARED_ARRAY_BUFFER_INTRINSIC: SharedArrayBufferIntrinsic =
    SharedArrayBufferIntrinsic;

pub(super) struct SharedArrayBufferIntrinsic;

impl IntrinsicInstaller for SharedArrayBufferIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = shared_array_buffer_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("SharedArrayBuffer class descriptor should normalize")
            .build();

        if let Some(ctor_desc) = plan.constructor() {
            let host_id = cx.native_functions.register(ctor_desc.clone());
            intrinsics.shared_array_buffer_constructor =
                cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype())?;
        }

        install_class_plan(
            intrinsics.shared_array_buffer_prototype,
            intrinsics.shared_array_buffer_constructor,
            &plan,
            intrinsics.function_prototype,
            cx,
        )?;

        install_getter(
            intrinsics.shared_array_buffer_prototype,
            "byteLength",
            shared_array_buffer_get_byte_length,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.shared_array_buffer_prototype,
            "growable",
            shared_array_buffer_get_growable,
            intrinsics,
            cx,
        )?;
        install_getter(
            intrinsics.shared_array_buffer_prototype,
            "maxByteLength",
            shared_array_buffer_get_max_byte_length,
            intrinsics,
            cx,
        )?;

        let tag_symbol = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag_str = cx.heap.alloc_string("SharedArrayBuffer");
        cx.heap.define_own_property(
            intrinsics.shared_array_buffer_prototype,
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
            "SharedArrayBuffer",
            RegisterValue::from_object_handle(intrinsics.shared_array_buffer_constructor.0),
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

fn shared_array_buffer_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("SharedArrayBuffer")
        .with_constructor(
            NativeFunctionDescriptor::constructor(
                "SharedArrayBuffer",
                1,
                shared_array_buffer_constructor,
            )
            .with_default_intrinsic(crate::intrinsics::IntrinsicKey::SharedArrayBufferPrototype),
        )
        .with_binding(proto("grow", 1, shared_array_buffer_grow))
        .with_binding(proto("slice", 2, shared_array_buffer_slice))
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
    error_message: &str,
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
        return Err(range_error(runtime, error_message));
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
        return Err(range_error(runtime, error_message));
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

fn require_shared_array_buffer_this(
    this: &RegisterValue,
    method_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not a SharedArrayBuffer"),
        )?);
    };
    if !matches!(
        runtime.objects().kind(handle),
        Ok(HeapValueKind::SharedArrayBuffer)
    ) {
        return Err(type_error(
            runtime,
            &format!("{method_name}: receiver is not a SharedArrayBuffer"),
        )?);
    }
    Ok(handle)
}

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
            "SharedArrayBuffer options must be an object",
        )?);
    };
    let property = runtime.intern_property_name("maxByteLength");
    let option = runtime.ordinary_get(options, property, value)?;
    if option == RegisterValue::undefined() {
        return Ok(None);
    }
    Ok(Some(to_index(
        option,
        runtime,
        "Invalid SharedArrayBuffer maxByteLength",
    )?))
}

// §25.2.3.1 SharedArrayBuffer(length[, options])
fn shared_array_buffer_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !runtime.is_current_native_construct_call() {
        return Err(type_error(
            runtime,
            "Constructor SharedArrayBuffer requires 'new'",
        )?);
    }

    let byte_length = to_index(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
        "Invalid SharedArrayBuffer length",
    )?;
    let requested_max = get_max_byte_length_option(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let (max_byte_length, growable) = match requested_max {
        Some(max_byte_length) => {
            if max_byte_length < byte_length {
                return Err(range_error(
                    runtime,
                    "SharedArrayBuffer maxByteLength must be >= byteLength",
                ));
            }
            (max_byte_length, true)
        }
        None => (byte_length, false),
    };

    let prototype = Some(runtime.intrinsics().shared_array_buffer_prototype);
    let handle = runtime.objects_mut().alloc_shared_array_buffer(
        byte_length,
        max_byte_length,
        growable,
        prototype,
    );
    Ok(RegisterValue::from_object_handle(handle.0))
}

// §25.2.5.1 get SharedArrayBuffer.prototype.byteLength
fn shared_array_buffer_get_byte_length(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_shared_array_buffer_this(
        this,
        "get SharedArrayBuffer.prototype.byteLength",
        runtime,
    )?;
    let byte_length = runtime
        .objects()
        .shared_array_buffer_byte_length(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_number(byte_length as f64))
}

// §25.2.5.4 get SharedArrayBuffer.prototype.growable
fn shared_array_buffer_get_growable(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_shared_array_buffer_this(
        this,
        "get SharedArrayBuffer.prototype.growable",
        runtime,
    )?;
    let growable = runtime
        .objects()
        .shared_array_buffer_is_growable(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_bool(growable))
}

// §25.2.5.5 get SharedArrayBuffer.prototype.maxByteLength
fn shared_array_buffer_get_max_byte_length(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = require_shared_array_buffer_this(
        this,
        "get SharedArrayBuffer.prototype.maxByteLength",
        runtime,
    )?;
    let max_byte_length = runtime
        .objects()
        .shared_array_buffer_max_byte_length(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_number(max_byte_length as f64))
}

// §25.2.5.3 SharedArrayBuffer.prototype.grow(newLength)
fn shared_array_buffer_grow(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle =
        require_shared_array_buffer_this(this, "SharedArrayBuffer.prototype.grow", runtime)?;
    let growable = runtime
        .objects()
        .shared_array_buffer_is_growable(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    if !growable {
        return Err(type_error(
            runtime,
            "SharedArrayBuffer.prototype.grow requires a growable SharedArrayBuffer",
        )?);
    }

    let new_byte_length = to_index(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
        "Invalid SharedArrayBuffer length",
    )?;
    runtime
        .objects_mut()
        .shared_array_buffer_grow(handle, new_byte_length)
        .map_err(|error| match error {
            crate::object::ObjectError::InvalidArrayLength => {
                range_error(runtime, "Invalid SharedArrayBuffer grow length")
            }
            other => VmNativeCallError::Internal(format!("{other:?}").into()),
        })?;
    Ok(RegisterValue::undefined())
}

// §25.2.5.6 SharedArrayBuffer.prototype.slice(start, end)
fn shared_array_buffer_slice(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle =
        require_shared_array_buffer_this(this, "SharedArrayBuffer.prototype.slice", runtime)?;
    let len = runtime
        .objects()
        .shared_array_buffer_byte_length(handle)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?
        as f64;

    let relative_start = to_integer_or_infinity(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let first = if relative_start == f64::NEG_INFINITY {
        0usize
    } else if relative_start < 0.0 {
        ((len + relative_start).max(0.0)) as usize
    } else {
        relative_start.min(len) as usize
    };

    let relative_end = if let Some(end) = args.get(1).copied() {
        to_integer_or_infinity(end, runtime)?
    } else {
        len
    };
    let final_ = if relative_end == f64::NEG_INFINITY {
        0usize
    } else if relative_end < 0.0 {
        ((len + relative_end).max(0.0)) as usize
    } else {
        relative_end.min(len) as usize
    };
    let new_len = final_.saturating_sub(first);

    let prototype = Some(runtime.intrinsics().shared_array_buffer_prototype);
    let new_handle = runtime
        .objects_mut()
        .shared_array_buffer_slice(handle, first, final_, new_len, false, prototype)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(RegisterValue::from_object_handle(new_handle.0))
}
