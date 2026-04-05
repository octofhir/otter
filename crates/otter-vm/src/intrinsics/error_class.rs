//! Error constructor intrinsics: Error, TypeError, ReferenceError, RangeError, SyntaxError.
//!
//! Each NativeError follows the same pattern as other built-in classes:
//! - Uses `JsClassDescriptor` + `ClassBuilder` for constructor/prototype setup
//! - Implements `IntrinsicInstaller` trait
//! - Constructor sets `message` property on the receiver

use crate::builders::ClassBuilder;
use crate::descriptors::{JsClassDescriptor, NativeFunctionDescriptor, VmNativeCallError};
use crate::object::ObjectHandle;
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static ERROR_INTRINSIC: ErrorIntrinsic = ErrorIntrinsic;

pub(super) struct ErrorIntrinsic;

impl IntrinsicInstaller for ErrorIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        // Install each error type using the same class builder pattern.
        install_error_class(
            "Error",
            intrinsics.error_prototype,
            &mut intrinsics.error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "TypeError",
            intrinsics.type_error_prototype,
            &mut intrinsics.type_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "ReferenceError",
            intrinsics.reference_error_prototype,
            &mut intrinsics.reference_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "RangeError",
            intrinsics.range_error_prototype,
            &mut intrinsics.range_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "SyntaxError",
            intrinsics.syntax_error_prototype,
            &mut intrinsics.syntax_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "URIError",
            intrinsics.uri_error_prototype,
            &mut intrinsics.uri_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "EvalError",
            intrinsics.eval_error_prototype,
            &mut intrinsics.eval_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;

        // §20.5.7 AggregateError — special constructor with (errors, message) signature.
        install_aggregate_error_class(
            intrinsics.aggregate_error_prototype,
            &mut intrinsics.aggregate_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;

        // Install Error.prototype.toString on the base Error prototype.
        install_error_to_string(intrinsics, cx)?;

        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let globals: &[(&str, ObjectHandle)] = &[
            ("Error", intrinsics.error_constructor),
            ("TypeError", intrinsics.type_error_constructor),
            ("ReferenceError", intrinsics.reference_error_constructor),
            ("RangeError", intrinsics.range_error_constructor),
            ("SyntaxError", intrinsics.syntax_error_constructor),
            ("URIError", intrinsics.uri_error_constructor),
            ("EvalError", intrinsics.eval_error_constructor),
            ("AggregateError", intrinsics.aggregate_error_constructor),
        ];
        for &(name, handle) in globals {
            cx.install_global_value(
                intrinsics,
                name,
                RegisterValue::from_object_handle(handle.0),
            )?;
        }
        Ok(())
    }
}

fn error_class_descriptor(name: &str) -> JsClassDescriptor {
    JsClassDescriptor::new(name).with_constructor(NativeFunctionDescriptor::constructor(
        name,
        1,
        error_constructor,
    ))
}

fn install_error_class(
    name: &str,
    prototype: ObjectHandle,
    constructor: &mut ObjectHandle,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = error_class_descriptor(name);
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("Error class descriptor should normalize")
        .build();

    // Replace the pre-allocated constructor with a real host function.
    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        let new_ctor = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
        *constructor = new_ctor;
    }

    install_class_plan(prototype, *constructor, &plan, function_prototype, cx)?;

    // Set prototype.name = error type name.
    let name_prop = cx.property_names.intern("name");
    let name_str = cx.heap.alloc_string(name);
    cx.heap.set_property(
        prototype,
        name_prop,
        RegisterValue::from_object_handle(name_str.0),
    )?;

    // Set prototype.message = "" (default empty message).
    let message_prop = cx.property_names.intern("message");
    let empty_str = cx.heap.alloc_string("");
    cx.heap.set_property(
        prototype,
        message_prop,
        RegisterValue::from_object_handle(empty_str.0),
    )?;

    Ok(())
}

/// §20.5.7 AggregateError — `new AggregateError(errors, message)`
/// Spec: <https://tc39.es/ecma262/#sec-aggregate-error-objects>
///
/// AggregateError stores an `errors` iterable as an own Array property
/// in addition to the standard `message` property.
fn install_aggregate_error_class(
    prototype: ObjectHandle,
    constructor: &mut ObjectHandle,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = JsClassDescriptor::new("AggregateError").with_constructor(
        NativeFunctionDescriptor::constructor("AggregateError", 2, aggregate_error_constructor),
    );
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("AggregateError class descriptor should normalize")
        .build();

    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        let new_ctor = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
        *constructor = new_ctor;
    }

    install_class_plan(prototype, *constructor, &plan, function_prototype, cx)?;

    // Set prototype.name = "AggregateError".
    let name_prop = cx.property_names.intern("name");
    let name_str = cx.heap.alloc_string("AggregateError");
    cx.heap.set_property(
        prototype,
        name_prop,
        RegisterValue::from_object_handle(name_str.0),
    )?;

    // Set prototype.message = "" (default empty message).
    let message_prop = cx.property_names.intern("message");
    let empty_str = cx.heap.alloc_string("");
    cx.heap.set_property(
        prototype,
        message_prop,
        RegisterValue::from_object_handle(empty_str.0),
    )?;

    Ok(())
}

/// §20.5.7.1 AggregateError ( errors, message )
/// Spec: <https://tc39.es/ecma262/#sec-aggregate-error>
fn aggregate_error_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("AggregateError constructor requires new".into())
    })?;

    // 1. Set message property if provided and not undefined (§20.5.7.1 step 4).
    let message_arg = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    if message_arg != RegisterValue::undefined() {
        let msg_str = runtime.js_to_string_infallible(message_arg);
        let msg_handle = runtime.alloc_string(msg_str);
        let msg_prop = runtime.intern_property_name("message");
        runtime
            .objects_mut()
            .set_property(
                handle,
                msg_prop,
                RegisterValue::from_object_handle(msg_handle.0),
            )
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    // 2. Store errors on the `errors` property (§20.5.7.1 step 5).
    //    Spec calls for IterableToList(errors), but storing the argument directly
    //    covers the common case of passing an array.
    let errors_arg = args.first().copied().unwrap_or(RegisterValue::undefined());
    let errors_prop = runtime.intern_property_name("errors");
    runtime
        .objects_mut()
        .set_property(handle, errors_prop, errors_arg)
        .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;

    Ok(*this)
}

/// Install `Error.prototype.toString` as a host method.
fn install_error_to_string(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method("toString", 0, error_to_string);
    let host_id = cx.native_functions.register(desc);
    let method = cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype)?;
    let prop = cx.property_names.intern("toString");
    cx.heap.set_property(
        intrinsics.error_prototype,
        prop,
        RegisterValue::from_object_handle(method.0),
    )?;
    Ok(())
}

/// ES2024 §20.5.3.4 Error.prototype.toString()
fn error_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Error.prototype.toString requires object".into())
    })?;

    // 1. Let name be ? Get(O, "name").
    let name_prop = runtime.intern_property_name("name");
    let name_val = runtime
        .ordinary_get(handle, name_prop, *this)
        .map_err(|e| match e {
            VmNativeCallError::Thrown(v) => VmNativeCallError::Thrown(v),
            other => other,
        })?;
    // 2. If name is undefined, set name to "Error"; else set name to ? ToString(name).
    let name = if name_val == RegisterValue::undefined() {
        "Error".to_string()
    } else {
        runtime
            .js_to_string(name_val)
            .map_err(|e| match e {
                crate::interpreter::InterpreterError::UncaughtThrow(v) => {
                    VmNativeCallError::Thrown(v)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            })?
            .into_string()
    };

    // 3. Let msg be ? Get(O, "message").
    let msg_prop = runtime.intern_property_name("message");
    let msg_val = runtime
        .ordinary_get(handle, msg_prop, *this)
        .map_err(|e| match e {
            VmNativeCallError::Thrown(v) => VmNativeCallError::Thrown(v),
            other => other,
        })?;
    // 4. If msg is undefined, set msg to ""; else set msg to ? ToString(msg).
    let msg = if msg_val == RegisterValue::undefined() {
        String::new()
    } else {
        runtime
            .js_to_string(msg_val)
            .map_err(|e| match e {
                crate::interpreter::InterpreterError::UncaughtThrow(v) => {
                    VmNativeCallError::Thrown(v)
                }
                other => VmNativeCallError::Internal(format!("{other}").into()),
            })?
            .into_string()
    };

    // 5. If name is "", return msg. 6. If msg is "", return name.
    // 7. Return name + ": " + msg.
    let result = if name.is_empty() {
        msg
    } else if msg.is_empty() {
        name
    } else {
        format!("{name}: {msg}")
    };

    let handle = runtime.alloc_string(result);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Shared constructor for all Error types.
/// `new Error("msg")` → object with message property.
/// Prototype is set by the `new` operator from Constructor.prototype.
fn error_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| VmNativeCallError::Internal("Error constructor requires new".into()))?;

    if let Some(msg_arg) = args.first()
        && *msg_arg != RegisterValue::undefined()
    {
        let msg_str = runtime.js_to_string_infallible(*msg_arg);
        let msg_handle = runtime.alloc_string(msg_str);
        let msg_prop = runtime.intern_property_name("message");
        runtime
            .objects_mut()
            .set_property(
                handle,
                msg_prop,
                RegisterValue::from_object_handle(msg_handle.0),
            )
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    // §20.5.1.1 step 5: InstallErrorCause(O, options)
    // Spec: <https://tc39.es/ecma262/#sec-installerrorcause>
    if let Some(options) = args.get(1)
        && let Some(opts_handle) = options.as_object_handle().map(ObjectHandle)
    {
        let cause_prop = runtime.intern_property_name("cause");
        if let Ok(Some(lookup)) = runtime.property_lookup(opts_handle, cause_prop)
            && let crate::object::PropertyValue::Data { value, .. } = lookup.value()
        {
            runtime
                .objects_mut()
                .set_property(handle, cause_prop, value)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        }
    }

    Ok(*this)
}
