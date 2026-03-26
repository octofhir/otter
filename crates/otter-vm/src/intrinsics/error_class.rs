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

    Ok(*this)
}
