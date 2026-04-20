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
/// Non-enumerable slot holding the captured `Vec<StackFrameInfo>` for an Error
/// instance. Read by the lazy `Error.prototype.stack` getter.
pub(crate) const ERROR_STACK_FRAMES_SLOT: &str = "__otter_error_stack_frames__";
/// Non-enumerable slot caching the materialized `.stack` string after the
/// first read. Cleared whenever a new frames slot is installed (e.g. via
/// `Error.captureStackTrace`).
pub(crate) const ERROR_STACK_STRING_SLOT: &str = "__otter_error_stack_string__";

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
        install_suppressed_error_class(
            intrinsics.suppressed_error_prototype,
            &mut intrinsics.suppressed_error_constructor,
            intrinsics.function_prototype,
            cx,
        )?;

        // Install Error.prototype.toString on the base Error prototype.
        install_error_to_string(intrinsics, cx)?;
        install_error_is_error(intrinsics, cx)?;
        install_error_stack_accessor(intrinsics, cx)?;
        install_error_capture_stack_trace(intrinsics, cx)?;

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
            ("SuppressedError", intrinsics.suppressed_error_constructor),
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

/// `SuppressedError` from explicit resource management. Stores the new
/// disposal error in `.error` and the older completion in `.suppressed`.
fn install_suppressed_error_class(
    prototype: ObjectHandle,
    constructor: &mut ObjectHandle,
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let descriptor = JsClassDescriptor::new("SuppressedError").with_constructor(
        NativeFunctionDescriptor::constructor("SuppressedError", 3, suppressed_error_constructor)
            .with_default_intrinsic(IntrinsicKey::SuppressedErrorPrototype),
    );
    let plan = ClassBuilder::from_descriptor(&descriptor)
        .expect("SuppressedError class descriptor should normalize")
        .build();

    if let Some(ctor_desc) = plan.constructor() {
        let host_id = cx.native_functions.register(ctor_desc.clone());
        let new_ctor = cx.alloc_intrinsic_host_function(host_id, function_prototype)?;
        install_function_length_name(new_ctor, ctor_desc.length(), ctor_desc.js_name(), cx)?;
        *constructor = new_ctor;
    }

    install_class_plan(prototype, *constructor, &plan, function_prototype, cx)?;

    let name_prop = cx.property_names.intern("name");
    let name_str = cx.heap.alloc_string("SuppressedError");
    cx.heap.define_own_property(
        prototype,
        name_prop,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(name_str.0),
            PropertyAttributes::from_flags(true, false, true),
        ),
    )?;

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
    capture_error_stack(runtime, handle, 0)?;

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

fn suppressed_error_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let error = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let suppressed = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let message = args
        .get(2)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    alloc_suppressed_error_value(runtime, error, suppressed, message)
}

pub(crate) fn alloc_suppressed_error_value(
    runtime: &mut crate::interpreter::RuntimeState,
    error: RegisterValue,
    suppressed: RegisterValue,
    message: RegisterValue,
) -> Result<RegisterValue, VmNativeCallError> {
    let handle = error_receiver_from_call(
        &RegisterValue::undefined(),
        runtime,
        runtime.intrinsics().suppressed_error_prototype,
    )?;
    install_error_brand(handle, runtime)?;
    capture_error_stack(runtime, handle, 0)?;

    if message != RegisterValue::undefined() {
        let msg = runtime
            .js_to_string(message)
            .map_err(|err| map_interpreter_error(err, runtime))?;
        let msg_handle = runtime.alloc_string(msg);
        define_non_enumerable_data_property(
            runtime,
            handle,
            "message",
            RegisterValue::from_object_handle(msg_handle.0),
        )?;
    }
    define_non_enumerable_data_property(runtime, handle, "error", error)?;
    define_non_enumerable_data_property(runtime, handle, "suppressed", suppressed)?;

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

/// Installs `Error.prototype.stack` as a V8-compatible accessor pair on the
/// base `Error.prototype`. The getter lazily formats the captured stack
/// snapshot the first time it is read; the setter lets user code overwrite
/// the value (matching V8 semantics).
///
/// V8 reference: <https://v8.dev/docs/stack-trace-api>
fn install_error_stack_accessor(
    intrinsics: &mut VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let getter_desc = NativeFunctionDescriptor::method("get stack", 0, error_stack_getter);
    let getter_id = cx.native_functions.register(getter_desc);
    let getter = cx.alloc_intrinsic_host_function(getter_id, intrinsics.function_prototype)?;
    install_function_length_name(getter, 0, "get stack", cx)?;

    let setter_desc = NativeFunctionDescriptor::method("set stack", 1, error_stack_setter);
    let setter_id = cx.native_functions.register(setter_desc);
    let setter = cx.alloc_intrinsic_host_function(setter_id, intrinsics.function_prototype)?;
    install_function_length_name(setter, 1, "set stack", cx)?;

    let stack_prop = cx.property_names.intern("stack");
    cx.heap.define_own_property(
        intrinsics.error_prototype,
        stack_prop,
        crate::object::PropertyValue::Accessor {
            getter: Some(getter),
            setter: Some(setter),
            attributes: crate::object::PropertyAttributes::from_flags(false, true, true),
        },
    )?;
    intrinsics.error_stack_getter = Some(getter);
    intrinsics.error_stack_setter = Some(setter);
    Ok(())
}

/// Installs V8-extension `Error.captureStackTrace(target, constructorOpt?)`
/// as a static method on the base `Error` constructor.
///
/// V8 reference: <https://v8.dev/docs/stack-trace-api#customizing-stack-traces>
fn install_error_capture_stack_trace(
    intrinsics: &VmIntrinsics,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    let desc = NativeFunctionDescriptor::method("captureStackTrace", 2, error_capture_stack_trace);
    let host_id = cx.native_functions.register(desc);
    let method = cx.alloc_intrinsic_host_function(host_id, intrinsics.function_prototype)?;
    install_function_length_name(method, 2, "captureStackTrace", cx)?;
    let prop = cx.property_names.intern("captureStackTrace");
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

/// V8-extension `Error.captureStackTrace(target, constructorOpt?)`.
///
/// Captures the current execution-context stack onto `target.stack`. If
/// `constructorOpt` is a function, all frames up to *and including* the
/// innermost frame whose closure handle matches `constructorOpt` are
/// dropped from the captured snapshot.
///
/// V8 reference: <https://v8.dev/docs/stack-trace-api>
fn error_capture_stack_trace(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(target) = args
        .first()
        .and_then(|value| value.as_object_handle().map(ObjectHandle))
    else {
        return Err(throw_type_error(
            runtime,
            "Error.captureStackTrace called on non-object",
        ));
    };

    // Determine how many frames to skip. Native callees (such as
    // captureStackTrace itself) never push a shadow-stack frame, so the
    // topmost shadow frame is already the caller — no default skip needed.
    let extra_skip = if let Some(ctor) = args
        .get(1)
        .and_then(|value| value.as_object_handle().map(ObjectHandle))
    {
        // Find the innermost (topmost) shadow frame whose closure handle
        // matches the user-supplied constructor. Skip everything at or
        // above that frame (inclusive).
        let stack = runtime.frame_info_stack_snapshot();
        let mut drop_count = 0usize;
        for (idx, frame) in stack.iter().enumerate().rev() {
            if frame.closure_handle == Some(ctor) {
                // Keep frames [0..idx), drop [idx..len).
                drop_count = stack.len() - idx;
                break;
            }
        }
        drop_count
    } else {
        0
    };

    capture_error_stack(runtime, target, extra_skip)?;

    // V8 also installs an own `stack` accessor on the target so that
    // `target.stack` works regardless of the target's prototype chain.
    let getter = runtime.intrinsics().error_stack_getter;
    let setter = runtime.intrinsics().error_stack_setter;
    if getter.is_some() || setter.is_some() {
        let stack_prop = runtime.intern_property_name("stack");
        runtime
            .objects_mut()
            .define_own_property(
                target,
                stack_prop,
                crate::object::PropertyValue::Accessor {
                    getter,
                    setter,
                    attributes: crate::object::PropertyAttributes::from_flags(false, true, true),
                },
            )
            .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
    }
    Ok(RegisterValue::undefined())
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
    capture_error_stack(runtime, handle, 0)?;

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

/// Captures the current execution-context stack and stores it as a hidden
/// non-enumerable slot on the given error object. Any cached materialised
/// `.stack` string is cleared so the next getter invocation re-formats.
///
/// `extra_skip` is the number of additional shadow-stack frames to drop from
/// the top of the captured stack. Used by `Error.captureStackTrace` when a
/// `constructorOpt` is supplied.
///
/// Note: native callees (such as `error_constructor` itself) do not push a
/// shadow-stack frame, so the topmost shadow frame at this point is the
/// caller of `new Error(...)`. We capture starting from that frame.
pub(crate) fn capture_error_stack(
    runtime: &mut crate::interpreter::RuntimeState,
    handle: ObjectHandle,
    extra_skip: usize,
) -> Result<(), VmNativeCallError> {
    let snapshot = runtime.capture_stack_snapshot(extra_skip);
    let frames_handle = runtime.objects_mut().alloc_error_stack_frames(snapshot);
    define_non_enumerable_data_property(
        runtime,
        handle,
        ERROR_STACK_FRAMES_SLOT,
        RegisterValue::from_object_handle(frames_handle.0),
    )?;
    // Clear any previously cached formatted string so the next getter call
    // reformats using the freshly captured frames.
    let cache_prop = runtime.intern_property_name(ERROR_STACK_STRING_SLOT);
    let _ = runtime.objects_mut().delete_property(handle, cache_prop);
    Ok(())
}

/// Reads `__otter_error_stack_frames__` off the error instance and returns
/// the heap handle to the frames bag (or `None` if the slot is missing).
fn read_stack_frames_slot(
    runtime: &mut crate::interpreter::RuntimeState,
    handle: ObjectHandle,
) -> Option<ObjectHandle> {
    let frames_prop = runtime.intern_property_name(ERROR_STACK_FRAMES_SLOT);
    let lookup = runtime
        .objects()
        .get_property(handle, frames_prop)
        .ok()
        .flatten()?;
    if lookup.owner() != handle {
        return None;
    }
    match lookup.value() {
        PropertyValue::Data { value, .. } => value.as_object_handle().map(ObjectHandle),
        _ => None,
    }
}

fn read_cached_stack_string(
    runtime: &mut crate::interpreter::RuntimeState,
    handle: ObjectHandle,
) -> Option<RegisterValue> {
    let cache_prop = runtime.intern_property_name(ERROR_STACK_STRING_SLOT);
    let lookup = runtime
        .objects()
        .get_property(handle, cache_prop)
        .ok()
        .flatten()?;
    if lookup.owner() != handle {
        return None;
    }
    match lookup.value() {
        PropertyValue::Data { value, .. } => Some(value),
        _ => None,
    }
}

/// Reads an Error instance's `name` property, falling back to the spec
/// default `"Error"` when missing/undefined.
fn read_error_name(
    runtime: &mut crate::interpreter::RuntimeState,
    handle: ObjectHandle,
) -> Result<String, VmNativeCallError> {
    let name_prop = runtime.intern_property_name("name");
    let name_val = runtime.ordinary_get(
        handle,
        name_prop,
        RegisterValue::from_object_handle(handle.0),
    )?;
    if name_val == RegisterValue::undefined() {
        Ok("Error".to_string())
    } else {
        runtime
            .js_to_string(name_val)
            .map(|s| s.into_string())
            .map_err(|error| map_interpreter_error(error, runtime))
    }
}

/// Reads an Error instance's `message` property, falling back to the empty
/// string when missing/undefined.
fn read_error_message(
    runtime: &mut crate::interpreter::RuntimeState,
    handle: ObjectHandle,
) -> Result<String, VmNativeCallError> {
    let msg_prop = runtime.intern_property_name("message");
    let msg_val = runtime.ordinary_get(
        handle,
        msg_prop,
        RegisterValue::from_object_handle(handle.0),
    )?;
    if msg_val == RegisterValue::undefined() {
        Ok(String::new())
    } else {
        runtime
            .js_to_string(msg_val)
            .map(|s| s.into_string())
            .map_err(|error| map_interpreter_error(error, runtime))
    }
}

/// V8-extension `Error.prototype.stack` getter. Lazily formats the captured
/// shadow-stack snapshot on first read and memoizes the result so subsequent
/// reads are O(1).
fn error_stack_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        return Ok(RegisterValue::undefined());
    };

    // Cached string wins.
    if let Some(cached) = read_cached_stack_string(runtime, handle) {
        return Ok(cached);
    }

    // No captured frames → return undefined (V8 leaves `.stack` undefined
    // for objects that were never passed through an Error constructor or
    // `captureStackTrace`).
    let Some(frames_handle) = read_stack_frames_slot(runtime, handle) else {
        return Ok(RegisterValue::undefined());
    };

    let name = read_error_name(runtime, handle)?;
    let message = read_error_message(runtime, handle)?;

    // Clone the frames out of the heap so we can drop the borrow before
    // allocating the formatted string.
    let frames = match runtime.objects().error_stack_frames(frames_handle) {
        Ok(Some(slice)) => slice.to_vec(),
        _ => return Ok(RegisterValue::undefined()),
    };

    let formatted = crate::stack_frame::format_v8_stack(&name, &message, &frames);
    let str_handle = runtime.alloc_string(formatted);
    let value = RegisterValue::from_object_handle(str_handle.0);

    // Memoize.
    define_non_enumerable_data_property(runtime, handle, ERROR_STACK_STRING_SLOT, value)?;
    Ok(value)
}

/// V8-extension `Error.prototype.stack` setter. Stores the user-supplied
/// value as the cached stack string and drops the underlying frame snapshot
/// so subsequent reads always return the caller's value verbatim.
fn error_stack_setter(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        return Ok(RegisterValue::undefined());
    };
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    define_non_enumerable_data_property(runtime, handle, ERROR_STACK_STRING_SLOT, value)?;
    // Drop the frames slot — the user's value now *is* the stack, and the
    // raw frames are no longer authoritative.
    let frames_prop = runtime.intern_property_name(ERROR_STACK_FRAMES_SLOT);
    let _ = runtime.objects_mut().delete_property(handle, frames_prop);
    Ok(RegisterValue::undefined())
}
