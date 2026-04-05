use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{
        IntrinsicInstallContext, IntrinsicInstaller, install_class_plan,
        install_function_length_name,
    },
};

pub(super) static FUNCTION_INTRINSIC: FunctionIntrinsic = FunctionIntrinsic;

pub(super) struct FunctionIntrinsic;

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

fn require_callable(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
    message: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    let handle = value
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, message).unwrap_or_else(|error| error))?;
    if !runtime.objects().is_callable(handle) {
        return Err(type_error(runtime, message)?);
    }
    Ok(handle)
}

fn list_from_apply_argument(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Vec<RegisterValue>, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Ok(Vec::new());
    }

    let handle = value.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        type_error(
            runtime,
            "Function.prototype.apply requires an object or null/undefined argArray",
        )
        .unwrap_or_else(|error| error)
    })?;
    if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
        return Err(type_error(
            runtime,
            "Function.prototype.apply requires an object or null/undefined argArray",
        )?);
    }

    runtime.list_from_array_like(handle)
}

fn target_function_length(
    target: ObjectHandle,
    bound_arg_count: usize,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<i32, VmNativeCallError> {
    let length = runtime.intern_property_name("length");
    let target_value = match runtime
        .own_property_descriptor(target, length)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("bound length lookup: {error:?}").into())
        })? {
        Some(_) => {
            runtime.ordinary_get(target, length, RegisterValue::from_object_handle(target.0))?
        }
        None => RegisterValue::undefined(),
    };
    let target_length = if let Some(value) = target_value.as_i32() {
        usize::try_from(value).unwrap_or(0)
    } else if let Some(value) = target_value.as_number() {
        if value.is_finite() && value >= 0.0 {
            value as usize
        } else {
            0
        }
    } else {
        0
    };
    Ok(i32::try_from(target_length.saturating_sub(bound_arg_count)).unwrap_or(i32::MAX))
}

fn target_function_name(
    target: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<String, VmNativeCallError> {
    let name = runtime.intern_property_name("name");
    let target_name = runtime
        .ordinary_get(target, name, RegisterValue::from_object_handle(target.0))?
        .as_object_handle()
        .map(ObjectHandle)
        .and_then(|handle| runtime.objects().string_value(handle).ok().flatten())
        .map(|text| text.to_string())
        .unwrap_or_default();
    Ok(format!("bound {target_name}"))
}

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

        // §10.2.8 SetFunctionLength + §10.2.9 SetFunctionName for Function constructor.
        install_function_length_name(constructor, 1, "Function", cx)?;

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

/// §20.2.1.1 Function(p1, p2, ..., pn, body)
///
/// Creates a new function from string arguments. All arguments except the last
/// are joined as the parameter list; the last argument is the function body.
/// The function is compiled and evaluated in the global scope.
///
/// Spec: <https://tc39.es/ecma262/#sec-function-p1-p2-pn-body>
fn function_constructor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // §20.2.1.1 Step 1-4: Collect parameter and body strings.
    let (params, body) = if args.is_empty() {
        (String::new(), String::new())
    } else if args.len() == 1 {
        let body_str = runtime
            .js_to_string(args[0])
            .map_err(|e| VmNativeCallError::Internal(format!("Function: {e}").into()))?;
        (String::new(), body_str.to_string())
    } else {
        let mut param_parts = Vec::with_capacity(args.len() - 1);
        for arg in &args[..args.len() - 1] {
            let s = runtime
                .js_to_string(*arg)
                .map_err(|e| VmNativeCallError::Internal(format!("Function: {e}").into()))?;
            param_parts.push(s.to_string());
        }
        let body_str = runtime
            .js_to_string(args[args.len() - 1])
            .map_err(|e| VmNativeCallError::Internal(format!("Function: {e}").into()))?;
        (param_parts.join(","), body_str.to_string())
    };

    // §20.2.1.1 Step 5-8: Build the source text and compile.
    let source = format!("(function anonymous({params}) {{\n{body}\n}})");
    let result = runtime.eval_source(&source, false, false)?;

    Ok(result)
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
        .map(|handle| runtime.objects().is_callable(handle))
        .unwrap_or(false);
    Ok(RegisterValue::from_bool(is_callable))
}

fn function_call(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callable = require_callable(
        *this,
        runtime,
        "Function.prototype.call requires callable receiver",
    )?;
    let receiver = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let forwarded = if args.len() > 1 { &args[1..] } else { &[] };

    runtime.call_callable(callable, receiver, forwarded)
}

/// ES2024 §20.2.3.1 Function.prototype.apply(thisArg, argArray)
fn function_apply(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callable = require_callable(
        *this,
        runtime,
        "Function.prototype.apply requires callable receiver",
    )?;
    let receiver = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let call_args = list_from_apply_argument(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    runtime.call_callable(callable, receiver, &call_args)
}

/// ES2024 §20.2.3.2 Function.prototype.bind(thisArg, ...args)
///
/// Creates a bound function that wraps the original with a fixed `this` and
/// optional prepended arguments.
fn function_bind(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = require_callable(
        *this,
        runtime,
        "Function.prototype.bind requires callable receiver",
    )?;
    let bound_this = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let bound_args: Vec<RegisterValue> = args.get(1..).unwrap_or(&[]).to_vec();
    let bound_length = target_function_length(target, bound_args.len(), runtime)?;
    let bound_name = target_function_name(target, runtime)?;

    // ES2024 §10.4.1.3 BoundFunctionCreate — create a proper bound function exotic object.
    let bound = runtime
        .objects_mut()
        .alloc_bound_function(target, bound_this, bound_args)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("bound function alloc: {error:?}").into())
        })?;

    let length_prop = runtime.intern_property_name("length");
    runtime
        .objects_mut()
        .define_own_property(
            bound,
            length_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_i32(bound_length),
                PropertyAttributes::function_length(),
            ),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(format!("bound function length install: {error:?}").into())
        })?;
    let name_prop = runtime.intern_property_name("name");
    let name_handle = runtime.alloc_string(bound_name);
    runtime
        .objects_mut()
        .define_own_property(
            bound,
            name_prop,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(name_handle.0),
                PropertyAttributes::function_length(),
            ),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(format!("bound function name install: {error:?}").into())
        })?;

    Ok(RegisterValue::from_object_handle(bound.0))
}

fn function_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let receiver = require_callable(
        *this,
        runtime,
        "Function.prototype.toString requires callable receiver",
    )?;

    let text = match runtime.objects().kind(receiver) {
        Ok(HeapValueKind::HostFunction) => "function () { [native code] }",
        Ok(HeapValueKind::Closure) => "function () { [bytecode] }",
        Ok(HeapValueKind::BoundFunction) => "function () { [native code] }",
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
