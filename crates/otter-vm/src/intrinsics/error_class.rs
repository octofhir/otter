//! Error constructor intrinsics: Error, TypeError, ReferenceError, RangeError, SyntaxError.
//!
//! Each NativeError follows the same pattern as other built-in classes:
//! - Uses `JsClassDescriptor` + `ClassBuilder` for constructor/prototype setup
//! - Implements `IntrinsicInstaller` trait
//! - Constructor sets `message` property on the receiver

use crate::builders::ClassBuilder;
use crate::descriptors::{JsClassDescriptor, NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicKey, IntrinsicsError, VmIntrinsics,
    install::{
        IntrinsicInstallContext, IntrinsicInstaller, install_class_plan,
        install_function_length_name,
    },
};

pub(super) static ERROR_INTRINSIC: ErrorIntrinsic = ErrorIntrinsic;
pub(crate) const ERROR_DATA_SLOT: &str = "__otter_error_data__";

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
            IntrinsicKey::ErrorPrototype,
            intrinsics.error_prototype,
            &mut intrinsics.error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "TypeError",
            IntrinsicKey::TypeErrorPrototype,
            intrinsics.type_error_prototype,
            &mut intrinsics.type_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "ReferenceError",
            IntrinsicKey::ReferenceErrorPrototype,
            intrinsics.reference_error_prototype,
            &mut intrinsics.reference_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "RangeError",
            IntrinsicKey::RangeErrorPrototype,
            intrinsics.range_error_prototype,
            &mut intrinsics.range_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "SyntaxError",
            IntrinsicKey::SyntaxErrorPrototype,
            intrinsics.syntax_error_prototype,
            &mut intrinsics.syntax_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "URIError",
            IntrinsicKey::URIErrorPrototype,
            intrinsics.uri_error_prototype,
            &mut intrinsics.uri_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;
        install_error_class(
            "EvalError",
            IntrinsicKey::EvalErrorPrototype,
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
        install_error_is_error(intrinsics, cx)?;

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

fn error_class_descriptor(name: &str, intrinsic_default: IntrinsicKey) -> JsClassDescriptor {
    JsClassDescriptor::new(name).with_constructor(
        NativeFunctionDescriptor::constructor(name, 1, error_constructor)
            .with_default_intrinsic(intrinsic_default),
    )
}

fn install_error_class(
    name: &str,
    intrinsic_default: IntrinsicKey,
    prototype: ObjectHandle,
    constructor: &mut ObjectHandle,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = error_class_descriptor(name, intrinsic_default);
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("Error class descriptor should normalize")
        .build();

    // Replace the pre-allocated constructor with a real host function.
    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        let new_ctor = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
        install_function_length_name(new_ctor, ctor_desc.length(), ctor_desc.js_name(), cx)?;
        *constructor = new_ctor;
    }

    install_class_plan(prototype, *constructor, &plan, function_prototype, cx)?;

    // Set prototype.name = error type name.
    let name_prop = cx.property_names.intern("name");
    let name_str = cx.heap.alloc_string(name);
    cx.heap.define_own_property(
        prototype,
        name_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(name_str.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;

    // Set prototype.message = "" (default empty message).
    let message_prop = cx.property_names.intern("message");
    let empty_str = cx.heap.alloc_string("");
    cx.heap.define_own_property(
        prototype,
        message_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(empty_str.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
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
        NativeFunctionDescriptor::constructor("AggregateError", 2, aggregate_error_constructor)
            .with_default_intrinsic(IntrinsicKey::AggregateErrorPrototype),
    );
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("AggregateError class descriptor should normalize")
        .build();

    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        let new_ctor = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
        install_function_length_name(new_ctor, ctor_desc.length(), ctor_desc.js_name(), cx)?;
        *constructor = new_ctor;
    }

    install_class_plan(prototype, *constructor, &plan, function_prototype, cx)?;

    // Set prototype.name = "AggregateError".
    let name_prop = cx.property_names.intern("name");
    let name_str = cx.heap.alloc_string("AggregateError");
    cx.heap.define_own_property(
        prototype,
        name_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(name_str.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;

    // Set prototype.message = "" (default empty message).
    let message_prop = cx.property_names.intern("message");
    let empty_str = cx.heap.alloc_string("");
    cx.heap.define_own_property(
        prototype,
        message_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(empty_str.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
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
    let handle = error_receiver_from_call(
        this,
        runtime,
        runtime.intrinsics().aggregate_error_prototype,
    )?;
    install_error_brand(handle, runtime)?;

    let message_arg = args.get(1).copied().unwrap_or(RegisterValue::undefined());
    if message_arg != RegisterValue::undefined() {
        let msg = runtime
            .js_to_string(message_arg)
            .map_err(|error| map_interpreter_error(error, runtime))?;
        let msg_handle = runtime.alloc_string(msg);
        define_non_enumerable_data_property(
            runtime,
            handle,
            "message",
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
    }

    let errors_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let errors_list =
        iterable_to_array(runtime, errors_arg, "AggregateError errors is not iterable")?;
    define_non_enumerable_data_property(
        runtime,
        handle,
        "errors",
        RegisterValue::from_object_handle(errors_list.0),
    )?;

    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Install `Error.prototype.toString` as a host method.
fn install_error_to_string(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method("toString", 0, error_to_string);
    let host_id = cx.native_functions.register(desc);
    let method = cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype)?;
    install_function_length_name(method, 0, "toString", cx)?;
    let prop = cx.property_names.intern("toString");
    cx.heap.define_own_property(
        intrinsics.error_prototype,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(method.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;
    Ok(())
}

fn install_error_is_error(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method("isError", 1, error_is_error);
    let host_id = cx.native_functions.register(desc);
    let method = cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype)?;
    install_function_length_name(method, 1, "isError", cx)?;
    let prop = cx.property_names.intern("isError");
    cx.heap.define_own_property(
        intrinsics.error_constructor,
        prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(method.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;
    Ok(())
}

/// ES2024 §20.5.3.4 Error.prototype.toString()
fn error_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .filter(|handle| {
            !matches!(
                runtime.objects().kind(*handle),
                Ok(HeapValueKind::String | HeapValueKind::BigInt)
            )
        })
        .ok_or_else(|| throw_type_error(runtime, "Error.prototype.toString requires an object"))?;

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
            .map_err(|error| map_interpreter_error(error, runtime))?
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
            .map_err(|error| map_interpreter_error(error, runtime))?
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
    let handle = error_receiver_from_call(this, runtime, runtime.intrinsics().error_prototype)?;
    install_error_brand(handle, runtime)?;

    if let Some(msg_arg) = args.first()
        && *msg_arg != RegisterValue::undefined()
    {
        let msg = runtime
            .js_to_string(*msg_arg)
            .map_err(|error| map_interpreter_error(error, runtime))?;
        let msg_handle = runtime.alloc_string(msg);
        define_non_enumerable_data_property(
            runtime,
            handle,
            "message",
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
    }

    // §20.5.1.1 step 5: InstallErrorCause(O, options)
    // Spec: <https://tc39.es/ecma262/#sec-installerrorcause>
    if let Some(options) = args.get(1)
        && let Some(opts_handle) = options.as_object_handle().map(ObjectHandle)
    {
        let cause_prop = runtime.intern_property_name("cause");
        let has_cause = if runtime.is_proxy(opts_handle) {
            runtime
                .proxy_has(opts_handle, cause_prop)
                .map_err(|error| map_interpreter_error(error, runtime))?
        } else {
            runtime
                .has_property(opts_handle, cause_prop)
                .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?
        };
        if has_cause {
            let value = runtime
                .ordinary_get(opts_handle, cause_prop, *options)
                .map_err(|error| match error {
                    VmNativeCallError::Thrown(value) => VmNativeCallError::Thrown(value),
                    other => other,
                })?;
            define_non_enumerable_data_property(runtime, handle, "cause", value)?;
        }
    }

    Ok(RegisterValue::from_object_handle(handle.0))
}

fn error_is_error(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle) = args
        .first()
        .and_then(|value| value.as_object_handle().map(ObjectHandle))
    else {
        return Ok(RegisterValue::from_bool(false));
    };

    Ok(RegisterValue::from_bool(has_error_brand(handle, runtime)?))
}

fn error_receiver_from_call(
    this: &RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
    default_prototype: ObjectHandle,
) -> Result<ObjectHandle, VmNativeCallError> {
    if runtime.is_current_native_construct_call() {
        return this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
            VmNativeCallError::Internal("Error constructor receiver missing".into())
        });
    }

    let mut prototype = default_prototype;
    if let Some(callee) = runtime.current_native_callee() {
        let callee_value = RegisterValue::from_object_handle(callee.0);
        let prototype_prop = runtime.intern_property_name("prototype");
        let prototype_value = if runtime.is_proxy(callee) {
            runtime
                .proxy_get(callee, prototype_prop, callee_value)
                .map_err(|error| map_interpreter_error(error, runtime))?
        } else {
            runtime.ordinary_get(callee, prototype_prop, callee_value)?
        };
        if let Some(handle) = prototype_value.as_object_handle().map(ObjectHandle) {
            prototype = handle;
        }
    }

    Ok(runtime.alloc_object_with_prototype(Some(prototype)))
}

fn iterable_to_array(
    runtime: &mut crate::interpreter::RuntimeState,
    iterable: RegisterValue,
    type_error_message: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let Some(iterable_handle) = iterable.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(runtime, type_error_message));
    };

    let iterator = get_iterator_object(runtime, iterable_handle, iterable)?;
    let result = runtime.alloc_array();
    let mut index = 0usize;
    loop {
        let (done, value) = runtime
            .call_iterator_next_with_value(iterator, RegisterValue::undefined())
            .map_err(|error| map_interpreter_error(error, runtime))?;
        if done {
            break;
        }
        runtime
            .objects_mut()
            .set_index(result, index, value)
            .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
        index += 1;
    }
    Ok(result)
}

fn get_iterator_object(
    runtime: &mut crate::interpreter::RuntimeState,
    iterable_handle: ObjectHandle,
    iterable_value: RegisterValue,
) -> Result<ObjectHandle, VmNativeCallError> {
    let iterator_symbol =
        runtime.intern_symbol_property_name(super::WellKnownSymbol::Iterator.stable_id());
    let iterator_method = runtime.ordinary_get(iterable_handle, iterator_symbol, iterable_value)?;

    if let Some(method) = iterator_method.as_object_handle().map(ObjectHandle)
        && runtime.objects().is_callable(method)
    {
        let iterator_value = runtime.call_callable(method, iterable_value, &[])?;
        return iterator_value
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| throw_type_error(runtime, "Iterator method returned a non-object"));
    }

    let next_prop = runtime.intern_property_name("next");
    let next_value = runtime.ordinary_get(iterable_handle, next_prop, iterable_value)?;
    if let Some(next_handle) = next_value.as_object_handle().map(ObjectHandle)
        && runtime.objects().is_callable(next_handle)
    {
        return Ok(iterable_handle);
    }

    Err(throw_type_error(runtime, "Value is not iterable"))
}

fn define_non_enumerable_data_property(
    runtime: &mut crate::interpreter::RuntimeState,
    handle: ObjectHandle,
    property_name: &str,
    value: RegisterValue,
) -> Result<(), VmNativeCallError> {
    let property = runtime.intern_property_name(property_name);
    runtime
        .objects_mut()
        .define_own_property(
            handle,
            property,
            PropertyValue::data_with_attrs(
                value,
                PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(())
}

fn install_error_brand(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let property = runtime.intern_property_name(ERROR_DATA_SLOT);
    runtime
        .objects_mut()
        .define_own_property(
            handle,
            property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_bool(true),
                PropertyAttributes::from_flags(false, false, false),
            ),
        )
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    Ok(())
}

fn has_error_brand(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<bool, VmNativeCallError> {
    let property = runtime.intern_property_name(ERROR_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, property)
        .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?
    else {
        return Ok(false);
    };

    Ok(lookup.owner() == handle && matches!(lookup.value(), PropertyValue::Data { .. }))
}

fn throw_type_error(
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> VmNativeCallError {
    match runtime.alloc_type_error(message) {
        Ok(error) => VmNativeCallError::Thrown(RegisterValue::from_object_handle(error.0)),
        Err(error) => {
            VmNativeCallError::Internal(format!("TypeError allocation failed: {error}").into())
        }
    }
}

fn map_interpreter_error(
    error: crate::interpreter::InterpreterError,
    runtime: &mut crate::interpreter::RuntimeState,
) -> VmNativeCallError {
    match error {
        crate::interpreter::InterpreterError::UncaughtThrow(value) => {
            VmNativeCallError::Thrown(value)
        }
        crate::interpreter::InterpreterError::TypeError(message) => {
            throw_type_error(runtime, &message)
        }
        crate::interpreter::InterpreterError::NativeCall(message) => {
            VmNativeCallError::Internal(message)
        }
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}
