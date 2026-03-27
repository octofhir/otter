use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static FUNCTION_INTRINSIC: FunctionIntrinsic = FunctionIntrinsic;

pub(super) struct FunctionIntrinsic;

impl IntrinsicInstaller for FunctionIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = function_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Function class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.function_constructor = constructor;
        install_class_plan(
            intrinsics.function_prototype(),
            intrinsics.function_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
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
            "Function",
            RegisterValue::from_object_handle(intrinsics.function_constructor().0),
        )
    }
}

fn function_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Function")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Function",
            1,
            function_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("call", 1, function_call),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isCallable", 1, function_is_callable),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("apply", 2, function_apply),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("bind", 1, function_bind),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, function_to_string),
        ))
}

fn function_constructor(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Err(VmNativeCallError::Internal(
        "Function constructor is not implemented in otter-vm bootstrap".into(),
    ))
}

fn function_is_callable(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let is_callable = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
        .map(|handle| {
            matches!(
                runtime.objects().kind(handle),
                Ok(HeapValueKind::HostFunction | HeapValueKind::Closure)
            )
        })
        .unwrap_or(false);
    Ok(RegisterValue::from_bool(is_callable))
}

fn function_call(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callable = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Function.prototype.call requires callable receiver".into())
    })?;
    let receiver = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let forwarded = if args.len() > 1 { &args[1..] } else { &[] };

    let Some(host_function) = runtime.objects().host_function(callable).map_err(|error| {
        VmNativeCallError::Internal(format!("Function.prototype.call failed: {error:?}").into())
    })?
    else {
        return Err(VmNativeCallError::Internal(
            "Function.prototype.call only supports host functions in otter-vm bootstrap".into(),
        ));
    };

    let descriptor = runtime
        .native_functions()
        .get(host_function)
        .cloned()
        .ok_or_else(|| VmNativeCallError::Internal("host function descriptor is missing".into()))?;

    (descriptor.callback())(&receiver, forwarded, runtime)
}

/// ES2024 §20.2.3.1 Function.prototype.apply(thisArg, argArray)
fn function_apply(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callable = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Function.prototype.apply requires callable receiver".into())
    })?;
    let receiver = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    // Extract args from argArray (second argument).
    let call_args = if let Some(arg_array) = args.get(1).copied()
        && let Some(handle) = arg_array.as_object_handle().map(ObjectHandle)
    {
        runtime.array_to_args(handle)?
    } else {
        Vec::new()
    };

    runtime.call_host_function(Some(callable), receiver, &call_args)
}

/// ES2024 §20.2.3.2 Function.prototype.bind(thisArg, ...args)
///
/// Creates a bound function that wraps the original with a fixed `this` and
/// optional prepended arguments. The bound function is a new host function
/// object that delegates to the original on invocation.
fn function_bind(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Function.prototype.bind requires callable receiver".into())
    })?;
    let bound_this = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let bound_args: Vec<RegisterValue> = args.get(1..).unwrap_or(&[]).to_vec();

    // ES2024 §10.4.1.3 BoundFunctionCreate — create a proper bound function exotic object.
    let bound = runtime
        .objects_mut()
        .alloc_bound_function(target, bound_this, bound_args);

    Ok(RegisterValue::from_object_handle(bound.0))
}

fn function_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Function.prototype.toString requires callable receiver".into())
    })?;

    let text = match runtime.objects().kind(receiver) {
        Ok(HeapValueKind::HostFunction) => "function () { [native code] }",
        Ok(HeapValueKind::Closure) => "function () { [bytecode] }",
        Ok(_) => {
            return Err(VmNativeCallError::Internal(
                "Function.prototype.toString requires callable receiver".into(),
            ));
        }
        Err(error) => {
            return Err(VmNativeCallError::Internal(
                format!("Function.prototype.toString failed: {error:?}").into(),
            ));
        }
    };

    let string = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(string.0))
}
