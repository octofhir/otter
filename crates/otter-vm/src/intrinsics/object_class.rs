use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{HeapValueKind, ObjectHandle, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    boolean_class::box_boolean_object,
    error_class::ERROR_DATA_SLOT,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
    number_class::box_number_object,
    string_class::box_string_object,
    symbol_class::box_symbol_object,
};

pub(super) static OBJECT_INTRINSIC: ObjectIntrinsic = ObjectIntrinsic;

const STRING_DATA_SLOT: &str = "__otter_string_data__";
const NUMBER_DATA_SLOT: &str = "__otter_number_data__";
const BOOLEAN_DATA_SLOT: &str = "__otter_boolean_data__";
const SYMBOL_DATA_SLOT: &str = "__otter_symbol_data__";
const OBJECT_IS_PROTOTYPE_OF_ERROR: &str =
    "Object.prototype.isPrototypeOf requires an object receiver";

pub(super) struct ObjectIntrinsic;

impl IntrinsicInstaller for ObjectIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = object_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Object class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.object_constructor = constructor;
        install_class_plan(
            intrinsics.object_prototype(),
            intrinsics.object_constructor(),
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
            "Object",
            RegisterValue::from_object_handle(intrinsics.object_constructor().0),
        )
    }
}

fn object_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Object")
        .with_constructor(
            NativeFunctionDescriptor::constructor("Object", 1, object_constructor)
                .with_default_intrinsic(crate::intrinsics::IntrinsicKey::ObjectPrototype),
        )
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toString", 0, object_to_string),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("isPrototypeOf", 1, object_is_prototype_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, object_value_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("create", 2, object_create),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "getOwnPropertyDescriptor",
                2,
                object_get_own_property_descriptor,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "getOwnPropertyDescriptors",
                1,
                object_get_own_property_descriptors,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("defineProperty", 3, object_define_property),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("defineProperties", 2, object_define_properties),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("is", 2, object_is),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("hasOwn", 2, object_has_own),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "getOwnPropertyNames",
                1,
                object_get_own_property_names,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method(
                "getOwnPropertySymbols",
                1,
                object_get_own_property_symbols,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("keys", 1, object_keys),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("values", 1, object_values),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("entries", 1, object_entries),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("freeze", 1, object_freeze),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isFrozen", 1, object_is_frozen),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("preventExtensions", 1, object_prevent_extensions),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("seal", 1, object_seal),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isSealed", 1, object_is_sealed),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("isExtensible", 1, object_is_extensible),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("getPrototypeOf", 1, object_get_prototype_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("setPrototypeOf", 2, object_set_prototype_of),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("assign", 2, object_assign),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("fromEntries", 1, object_from_entries),
        ))
        // §22.1.2.11 Object.groupBy(items, callbackfn)
        // <https://tc39.es/ecma262/#sec-object.groupby>
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("groupBy", 2, object_group_by),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("hasOwnProperty", 1, object_has_own_property),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method(
                "propertyIsEnumerable",
                1,
                object_property_is_enumerable,
            ),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("toLocaleString", 0, object_to_locale_string),
        ))
}

fn object_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if let Some(value) = args.first().copied() {
        if value == RegisterValue::undefined() || value == RegisterValue::null() {
            if this.as_object_handle().is_some() {
                return Ok(*this);
            }
            let object = runtime.alloc_object();
            return Ok(RegisterValue::from_object_handle(object.0));
        }

        if let Some(boolean) = value.as_bool() {
            return box_boolean_object(RegisterValue::from_bool(boolean), runtime);
        }

        if let Some(number) = value.as_number() {
            return box_number_object(RegisterValue::from_number(number), runtime);
        }

        if value.is_symbol() {
            return box_symbol_object(value, runtime);
        }

        if let Some(handle) = value.as_object_handle().map(ObjectHandle) {
            return match runtime.objects().kind(handle) {
                Ok(HeapValueKind::String) => box_string_object(handle, runtime),
                Ok(_) => Ok(value),
                Err(error) => Err(VmNativeCallError::Internal(
                    format!("Object constructor kind lookup failed: {error:?}").into(),
                )),
            };
        }
    }

    if this.as_object_handle().is_some() {
        return Ok(*this);
    }

    let object = runtime.alloc_object();
    Ok(RegisterValue::from_object_handle(object.0))
}

fn object_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let object = to_object_for_prototype_method(
        *this,
        runtime,
        "Object.prototype.valueOf requires an object-coercible receiver",
    )?;
    Ok(RegisterValue::from_object_handle(object.0))
}

fn object_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let tag = if *this == RegisterValue::undefined() || *this == RegisterValue::null() {
        object_to_string_tag(*this, runtime)?.to_string()
    } else {
        let object = to_object_for_prototype_method(
            *this,
            runtime,
            "Object.prototype.toString requires an object-coercible receiver",
        )?;
        let builtin_tag =
            object_to_string_tag(RegisterValue::from_object_handle(object.0), runtime)?;
        let to_string_tag =
            runtime.intern_symbol_property_name(WellKnownSymbol::ToStringTag.stable_id());
        let symbol_tag = runtime.ordinary_get(
            object,
            to_string_tag,
            RegisterValue::from_object_handle(object.0),
        )?;
        if let Some(handle) = symbol_tag.as_object_handle().map(ObjectHandle)
            && let Some(tag) = runtime.objects().string_value(handle).map_err(|error| {
                VmNativeCallError::Internal(
                    format!("Object.prototype.toString tag lookup failed: {error:?}").into(),
                )
            })?
        {
            tag.to_string()
        } else {
            let legacy_tag = runtime.intern_property_name("@@toStringTag");
            let tag_value = runtime.ordinary_get(
                object,
                legacy_tag,
                RegisterValue::from_object_handle(object.0),
            )?;
            match tag_value.as_object_handle().map(ObjectHandle) {
                Some(handle) => match runtime.objects().string_value(handle).map_err(|error| {
                    VmNativeCallError::Internal(
                        format!("Object.prototype.toString tag lookup failed: {error:?}").into(),
                    )
                })? {
                    Some(tag) => tag.to_string(),
                    None => builtin_tag.to_string(),
                },
                None => builtin_tag.to_string(),
            }
        }
    };
    let string = runtime.alloc_string(format!("[object {tag}]"));
    Ok(RegisterValue::from_object_handle(string.0))
}

fn object_is_prototype_of(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype = to_object_for_prototype_method(*this, runtime, OBJECT_IS_PROTOTYPE_OF_ERROR)?;
    let Some(mut candidate) = args
        .first()
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle)
    else {
        return Ok(RegisterValue::from_bool(false));
    };
    if matches!(runtime.objects().kind(candidate), Ok(HeapValueKind::String)) {
        return Ok(RegisterValue::from_bool(false));
    }

    while let Some(current) = runtime
        .objects()
        .get_prototype(candidate)
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("isPrototypeOf prototype lookup failed: {error:?}").into(),
            )
        })?
    {
        if current == prototype {
            return Ok(RegisterValue::from_bool(true));
        }
        candidate = current;
    }

    Ok(RegisterValue::from_bool(false))
}

fn object_create(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let prototype_arg = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let prototype = if prototype_arg == RegisterValue::null() {
        None
    } else {
        let handle = prototype_arg
            .as_object_handle()
            .map(ObjectHandle)
            .ok_or_else(|| {
                throw_type_error(runtime, "Object.create prototype must be an object or null")
                    .unwrap_or_else(|error| error)
            })?;
        if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
            return Err(throw_type_error(
                runtime,
                "Object.create prototype must be an object or null",
            )?);
        }
        Some(handle)
    };
    let object = runtime.alloc_object_with_prototype(prototype);

    if let Some(properties_arg) = args.get(1).copied()
        && properties_arg != RegisterValue::undefined()
    {
        let properties = to_object_for_descriptor_map(
            properties_arg,
            runtime,
            "Object.create properties must be object-coercible",
        )?;
        let descriptors = crate::abstract_ops::collect_define_properties(properties, runtime)?;
        let property_names = runtime.property_names().clone();
        for (property, descriptor) in descriptors {
            let success = runtime
                .objects_mut()
                .define_own_property_from_descriptor_with_registry(
                    object,
                    property,
                    descriptor,
                    &property_names,
                )
                .map_err(|e| VmNativeCallError::Internal(format!("Object.create: {e:?}").into()))?;
            if !success {
                return Err(throw_type_error(
                    runtime,
                    "Object.create could not define property",
                )?);
            }
        }
    }

    Ok(RegisterValue::from_object_handle(object.0))
}

fn to_object_for_introspection(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<ObjectHandle, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Err(throw_type_error(
            runtime,
            "Object operation requires an object-coercible value",
        )?);
    }

    if let Some(boolean) = value.as_bool() {
        let object = box_boolean_object(RegisterValue::from_bool(boolean), runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed boolean should return an object"),
        ));
    }

    if let Some(number) = value.as_number() {
        let object = box_number_object(RegisterValue::from_number(number), runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed number should return an object"),
        ));
    }

    if value.is_symbol() {
        let object = box_symbol_object(value, runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed symbol should return an object"),
        ));
    }

    value.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        throw_type_error(
            runtime,
            "Object operation requires an object-coercible value",
        )
        .unwrap_or_else(|error| error)
    })
}

fn non_string_object_target(
    value: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> Option<ObjectHandle> {
    let handle = value.as_object_handle().map(ObjectHandle)?;
    if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
        return None;
    }
    Some(handle)
}

fn with_vm_context(error: VmNativeCallError, context: &str) -> VmNativeCallError {
    match error {
        VmNativeCallError::Thrown(value) => VmNativeCallError::Thrown(value),
        VmNativeCallError::Internal(message) => {
            VmNativeCallError::Internal(format!("{context}: {message}").into())
        }
    }
}

fn to_object_for_descriptor_map(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
    context: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    match to_object_for_introspection(value, runtime) {
        Ok(object) => Ok(object),
        Err(VmNativeCallError::Thrown(_)) => Err(throw_type_error(runtime, context)?),
        Err(error) => Err(with_vm_context(error, context)),
    }
}

pub(super) fn to_object_for_prototype_method(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
    context: &str,
) -> Result<ObjectHandle, VmNativeCallError> {
    if value == RegisterValue::undefined() || value == RegisterValue::null() {
        return Err(throw_type_error(runtime, context)?);
    }

    if let Some(boolean) = value.as_bool() {
        let object = box_boolean_object(RegisterValue::from_bool(boolean), runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed boolean should return an object"),
        ));
    }

    if let Some(number) = value.as_number() {
        let object = box_number_object(RegisterValue::from_number(number), runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed number should return an object"),
        ));
    }

    if value.is_symbol() {
        let object = box_symbol_object(value, runtime)?;
        return Ok(ObjectHandle(
            object
                .as_object_handle()
                .expect("boxed symbol should return an object"),
        ));
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(runtime, context)?);
    };

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::String) => {
            let object = box_string_object(handle, runtime)?;
            Ok(ObjectHandle(
                object
                    .as_object_handle()
                    .expect("boxed string should return an object"),
            ))
        }
        Ok(_) => Ok(handle),
        Err(error) => Err(VmNativeCallError::Internal(
            format!("ToObject receiver kind lookup failed: {error:?}").into(),
        )),
    }
}

fn throw_type_error(
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

fn primitive_prototype(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<ObjectHandle>, VmNativeCallError> {
    if value.as_bool().is_some() {
        return Ok(Some(runtime.intrinsics().boolean_prototype()));
    }

    if value.as_number().is_some() {
        return Ok(Some(runtime.intrinsics().number_prototype()));
    }

    if value.is_symbol() {
        return Ok(Some(runtime.intrinsics().symbol_prototype()));
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Ok(None);
    };

    match runtime.objects().kind(handle) {
        Ok(HeapValueKind::String) => Ok(Some(runtime.intrinsics().string_prototype())),
        Ok(_) => Ok(None),
        Err(error) => Err(VmNativeCallError::Internal(
            format!("primitive prototype kind lookup failed: {error:?}").into(),
        )),
    }
}

fn prototype_argument(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<ObjectHandle>, VmNativeCallError> {
    if value == RegisterValue::null() {
        return Ok(None);
    }

    non_string_object_target(value, runtime)
        .map(Some)
        .ok_or_else(|| {
            throw_type_error(
                runtime,
                "Object.setPrototypeOf proto must be object or null",
            )
            .unwrap_or_else(|error| error)
        })
}

fn object_to_string_tag(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<&'static str, VmNativeCallError> {
    if value == RegisterValue::undefined() {
        return Ok("Undefined");
    }
    if value == RegisterValue::null() {
        return Ok("Null");
    }
    if value.as_bool().is_some() {
        return Ok("Boolean");
    }
    if value.as_number().is_some() {
        return Ok("Number");
    }
    if value.is_symbol() {
        return Ok("Symbol");
    }

    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Ok("Object");
    };

    if matches!(runtime.objects().kind(handle), Ok(HeapValueKind::String)) {
        return Ok("String");
    }
    if has_own_data_slot(handle, STRING_DATA_SLOT, runtime)? {
        return Ok("String");
    }
    if has_own_data_slot(handle, NUMBER_DATA_SLOT, runtime)? {
        return Ok("Number");
    }
    if has_own_data_slot(handle, BOOLEAN_DATA_SLOT, runtime)? {
        return Ok("Boolean");
    }
    if has_own_data_slot(handle, SYMBOL_DATA_SLOT, runtime)? {
        return Ok("Symbol");
    }
    if has_own_data_slot(handle, ERROR_DATA_SLOT, runtime)? {
        return Ok("Error");
    }

    match runtime.objects().kind(handle).map_err(|error| {
        VmNativeCallError::Internal(format!("toString kind lookup failed: {error:?}").into())
    })? {
        HeapValueKind::Array => Ok("Array"),
        HeapValueKind::HostFunction
        | HeapValueKind::Closure
        | HeapValueKind::BoundFunction
        | HeapValueKind::PromiseCapabilityFunction
        | HeapValueKind::PromiseCombinatorElement
        | HeapValueKind::PromiseFinallyFunction
        | HeapValueKind::PromiseValueThunk => Ok("Function"),
        HeapValueKind::Object
        | HeapValueKind::String
        | HeapValueKind::UpvalueCell
        | HeapValueKind::Iterator
        | HeapValueKind::Promise => Ok("Object"),
        HeapValueKind::Map => Ok("Map"),
        HeapValueKind::Set => Ok("Set"),
        HeapValueKind::MapIterator => Ok("Map Iterator"),
        HeapValueKind::SetIterator => Ok("Set Iterator"),
        HeapValueKind::WeakMap => Ok("WeakMap"),
        HeapValueKind::WeakSet => Ok("WeakSet"),
        HeapValueKind::WeakRef => Ok("WeakRef"),
        HeapValueKind::FinalizationRegistry => Ok("FinalizationRegistry"),
        HeapValueKind::Generator => Ok("Generator"),
        HeapValueKind::AsyncGenerator => Ok("AsyncGenerator"),
        HeapValueKind::ArrayBuffer => Ok("ArrayBuffer"),
        HeapValueKind::SharedArrayBuffer => Ok("SharedArrayBuffer"),
        HeapValueKind::RegExp => Ok("RegExp"),
        HeapValueKind::Proxy => Ok("Object"),
        HeapValueKind::TypedArray => Ok("TypedArray"),
        HeapValueKind::DataView => Ok("DataView"),
        HeapValueKind::BigInt => Ok("BigInt"),
        HeapValueKind::ErrorStackFrames => Ok("Object"),
    }
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.8  Object.getOwnPropertyDescriptor(O, P)
// ---------------------------------------------------------------------------
fn object_get_own_property_descriptor(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let key = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;

    // §10.5.5 — Proxy [[GetOwnProperty]] trap
    let descriptor = if runtime.is_proxy(target) {
        runtime
            .proxy_get_own_property_descriptor(target, property)
            .map_err(interp_to_native)?
    } else {
        runtime
            .own_property_descriptor(target, property)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("getOwnPropertyDescriptor: {e:?}").into())
            })?
    };

    let Some(descriptor) = descriptor else {
        return Ok(RegisterValue::undefined());
    };

    crate::abstract_ops::from_property_descriptor(descriptor, runtime)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.9  Object.getOwnPropertyDescriptors(O)
// ---------------------------------------------------------------------------
fn object_get_own_property_descriptors(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // §10.5.11 / §10.5.5 — Proxy trap dispatch
    let keys = if runtime.is_proxy(target) {
        runtime.proxy_own_keys(target).map_err(interp_to_native)?
    } else {
        runtime.own_property_keys(target).map_err(|e| {
            VmNativeCallError::Internal(format!("Object.getOwnPropertyDescriptors: {e:?}").into())
        })?
    };
    let result = runtime.alloc_object();

    for key in keys {
        let descriptor = if runtime.is_proxy(target) {
            runtime
                .proxy_get_own_property_descriptor(target, key)
                .map_err(interp_to_native)?
        } else {
            runtime.own_property_descriptor(target, key).map_err(|e| {
                VmNativeCallError::Internal(
                    format!("Object.getOwnPropertyDescriptors descriptor: {e:?}").into(),
                )
            })?
        };
        let Some(descriptor) = descriptor else {
            continue;
        };
        let descriptor_object = crate::abstract_ops::from_property_descriptor(descriptor, runtime)?;
        runtime
            .objects_mut()
            .set_property(result, key, descriptor_object)
            .map_err(|e| {
                VmNativeCallError::Internal(
                    format!("Object.getOwnPropertyDescriptors result store: {e:?}").into(),
                )
            })?;
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.4  Object.defineProperty(O, P, Attributes)
// ---------------------------------------------------------------------------
fn object_define_property(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(|value| non_string_object_target(value, runtime))
        .ok_or_else(|| {
            throw_type_error(runtime, "Object.defineProperty requires an object")
                .unwrap_or_else(|error| error)
        })?;
    let key = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;
    let desc_obj = args
        .get(2)
        .copied()
        .and_then(RegisterValue::as_object_handle)
        .map(ObjectHandle);

    // §10.5.6 — Proxy [[DefineOwnProperty]] trap
    if runtime.is_proxy(target) {
        let desc_value = args
            .get(2)
            .copied()
            .unwrap_or_else(RegisterValue::undefined);
        let success = runtime
            .proxy_define_own_property(target, property, desc_value)
            .map_err(interp_to_native)?;
        if !success {
            return Err(throw_type_error(
                runtime,
                "Object.defineProperty could not define property",
            )?);
        }
        return Ok(RegisterValue::from_object_handle(target.0));
    }

    let desc = crate::abstract_ops::to_property_descriptor(desc_obj, runtime)?;
    let property_names = runtime.property_names().clone();
    let success = runtime
        .objects_mut()
        .define_own_property_from_descriptor_with_registry(target, property, desc, &property_names)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.defineProperty: {e:?}").into()))?;

    if !success {
        return Err(throw_type_error(
            runtime,
            "Object.defineProperty could not define property",
        )?);
    }

    Ok(RegisterValue::from_object_handle(target.0))
}

// ES2024 §20.1.2.3  Object.defineProperties(O, Properties)
// ---------------------------------------------------------------------------
fn object_define_properties(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .and_then(|value| non_string_object_target(value, runtime))
        .ok_or_else(|| {
            throw_type_error(runtime, "Object.defineProperties requires an object")
                .unwrap_or_else(|error| error)
        })?;
    let properties_value = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let properties = to_object_for_descriptor_map(
        properties_value,
        runtime,
        "Object.defineProperties requires an object-coercible descriptor map",
    )?;
    let descriptors = crate::abstract_ops::collect_define_properties(properties, runtime)?;
    let property_names = runtime.property_names().clone();
    for (property, descriptor) in descriptors {
        let result = runtime
            .objects_mut()
            .define_own_property_from_descriptor_with_registry(
                target,
                property,
                descriptor,
                &property_names,
            );
        let success = match result {
            Ok(ok) => ok,
            Err(crate::object::ObjectError::OutOfMemory) => {
                // Heap cap hit while growing the dense element vector
                // (e.g. `defineProperties(arr, { "4294967294": ... })`).
                // Surface as a catchable `RangeError` per the same pattern
                // used by Array.prototype methods and `set_array_length`.
                return Err(runtime.throw_range_error("out of memory: heap limit exceeded"));
            }
            Err(crate::object::ObjectError::InvalidArrayLength) => {
                return Err(runtime.throw_range_error("Invalid array length"));
            }
            Err(e) => {
                return Err(VmNativeCallError::Internal(
                    format!("Object.defineProperties: {e:?}").into(),
                ));
            }
        };
        if !success {
            return Err(throw_type_error(
                runtime,
                "Object.defineProperties could not define property",
            )?);
        }
    }

    Ok(RegisterValue::from_object_handle(target.0))
}

/// ES2024 §20.1.2.14 Object.is(value1, value2)
fn object_is(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let lhs = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let rhs = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let same = runtime
        .objects()
        .same_value(lhs, rhs)
        .map_err(|error| VmNativeCallError::Internal(format!("Object.is: {error:?}").into()))?;
    Ok(RegisterValue::from_bool(same))
}

fn object_has_own(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;
    let property = runtime.property_name_from_value(
        args.get(1)
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
    )?;
    let has = runtime
        .own_property_descriptor(target, property)
        .map_err(|error| VmNativeCallError::Internal(format!("Object.hasOwn: {error:?}").into()))?
        .is_some();
    Ok(RegisterValue::from_bool(has))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.17  Object.keys(O)
// ---------------------------------------------------------------------------
fn object_keys(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // §10.5.11 — Proxy [[OwnPropertyKeys]] trap (filtered to enumerable string keys)
    let keys = if runtime.is_proxy(target) {
        proxy_enumerable_own_keys(runtime, target)?
    } else {
        runtime
            .enumerable_own_property_keys(target)
            .map_err(|e| VmNativeCallError::Internal(format!("Object.keys: {e}").into()))?
    };

    let array = runtime.alloc_array();
    for key_id in &keys {
        let name = runtime
            .property_names()
            .get(*key_id)
            .unwrap_or("")
            .to_string();
        let str_handle = runtime.alloc_string(name);
        runtime
            .objects_mut()
            .push_element(array, RegisterValue::from_object_handle(str_handle.0))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.22  Object.values(O)
// ---------------------------------------------------------------------------
fn object_values(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    let keys = if runtime.is_proxy(target) {
        proxy_enumerable_own_keys(runtime, target)?
    } else {
        runtime
            .enumerable_own_property_keys(target)
            .map_err(|e| VmNativeCallError::Internal(format!("Object.values: {e}").into()))?
    };

    let array = runtime.alloc_array();
    for key_id in &keys {
        let value = runtime
            .own_property_value(target, *key_id)
            .map_err(|e| with_vm_context(e, "Object.values get"))?;
        runtime.objects_mut().push_element(array, value).ok();
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.5  Object.entries(O)
// ---------------------------------------------------------------------------
fn object_entries(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    let keys = if runtime.is_proxy(target) {
        proxy_enumerable_own_keys(runtime, target)?
    } else {
        runtime
            .enumerable_own_property_keys(target)
            .map_err(|e| VmNativeCallError::Internal(format!("Object.entries: {e}").into()))?
    };

    let result = runtime.alloc_array();
    for key_id in &keys {
        let value = runtime
            .own_property_value(target, *key_id)
            .map_err(|e| with_vm_context(e, "Object.entries get"))?;
        let name = runtime
            .property_names()
            .get(*key_id)
            .unwrap_or("")
            .to_string();
        let key_str = runtime.alloc_string(name);
        let pair = runtime.alloc_array();
        runtime
            .objects_mut()
            .push_element(pair, RegisterValue::from_object_handle(key_str.0))
            .ok();
        runtime.objects_mut().push_element(pair, value).ok();
        runtime
            .objects_mut()
            .push_element(result, RegisterValue::from_object_handle(pair.0))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.6  Object.freeze(O)
// ---------------------------------------------------------------------------
fn object_freeze(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    // Non-object arguments are returned as-is per spec.
    let Some(handle) = non_string_object_target(target, runtime) else {
        return Ok(target);
    };
    runtime
        .objects_mut()
        .freeze(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.freeze: {e:?}").into()))?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.19  Object.seal(O)
// ---------------------------------------------------------------------------
fn object_seal(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = non_string_object_target(target, runtime) else {
        return Ok(target);
    };
    runtime
        .objects_mut()
        .seal(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.seal: {e:?}").into()))?;
    Ok(target)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.17 Object.preventExtensions(O)
// ---------------------------------------------------------------------------
fn object_prevent_extensions(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = non_string_object_target(target, runtime) else {
        return Ok(target);
    };
    // §10.5.4 — Proxy [[PreventExtensions]] trap
    if runtime.is_proxy(handle) {
        runtime
            .proxy_prevent_extensions(handle)
            .map_err(interp_to_native)?;
    } else {
        runtime
            .objects_mut()
            .prevent_extensions(handle)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("Object.preventExtensions: {e:?}").into())
            })?;
    }
    Ok(target)
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.15 Object.isExtensible(O)
// ---------------------------------------------------------------------------
fn object_is_extensible(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = non_string_object_target(target, runtime) else {
        return Ok(RegisterValue::from_bool(false));
    };
    // §10.5.3 — Proxy [[IsExtensible]] trap
    let extensible = if runtime.is_proxy(handle) {
        runtime
            .proxy_is_extensible(handle)
            .map_err(interp_to_native)?
    } else {
        runtime.objects().is_extensible(handle).map_err(|e| {
            VmNativeCallError::Internal(format!("Object.isExtensible: {e:?}").into())
        })?
    };
    Ok(RegisterValue::from_bool(extensible))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.x Object.isFrozen(O)
// ---------------------------------------------------------------------------
fn object_is_frozen(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = non_string_object_target(target, runtime) else {
        return Ok(RegisterValue::from_bool(true));
    };
    let frozen = runtime
        .objects()
        .is_frozen(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.isFrozen: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(frozen))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.x Object.isSealed(O)
// ---------------------------------------------------------------------------
fn object_is_sealed(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let Some(handle) = non_string_object_target(target, runtime) else {
        return Ok(RegisterValue::from_bool(true));
    };
    let sealed = runtime
        .objects()
        .is_sealed(handle)
        .map_err(|e| VmNativeCallError::Internal(format!("Object.isSealed: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(sealed))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.14  Object.getPrototypeOf(O)
// ---------------------------------------------------------------------------
fn object_get_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if target == RegisterValue::undefined() || target == RegisterValue::null() {
        return Err(throw_type_error(
            runtime,
            "Object.getPrototypeOf requires an object-coercible value",
        )?);
    }

    if let Some(proto) = primitive_prototype(target, runtime)? {
        return Ok(RegisterValue::from_object_handle(proto.0));
    }

    let target = target.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Object.getPrototypeOf target kind invalid".into())
    })?;
    // §10.5.1 — Proxy [[GetPrototypeOf]] trap
    let proto = if runtime.is_proxy(target) {
        runtime
            .proxy_get_prototype_of(target)
            .map_err(interp_to_native)?
    } else {
        runtime.objects().get_prototype(target).map_err(|e| {
            VmNativeCallError::Internal(format!("Object.getPrototypeOf: {e:?}").into())
        })?
    };
    Ok(proto
        .map(|h| RegisterValue::from_object_handle(h.0))
        .unwrap_or_else(RegisterValue::null))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.20  Object.setPrototypeOf(O, proto)
// ---------------------------------------------------------------------------
fn object_set_prototype_of(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let proto_arg = args
        .get(1)
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let proto = prototype_argument(proto_arg, runtime)?;
    if target == RegisterValue::undefined() || target == RegisterValue::null() {
        return Err(throw_type_error(
            runtime,
            "Object.setPrototypeOf requires an object-coercible value",
        )?);
    }

    if primitive_prototype(target, runtime)?.is_some() {
        return Ok(target);
    }

    let target = target.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Object.setPrototypeOf target kind invalid".into())
    })?;
    // §10.5.2 — Proxy [[SetPrototypeOf]] trap
    let success = if runtime.is_proxy(target) {
        runtime
            .proxy_set_prototype_of(target, proto)
            .map_err(interp_to_native)?
    } else {
        runtime
            .objects_mut()
            .set_prototype(target, proto)
            .map_err(|e| {
                VmNativeCallError::Internal(format!("Object.setPrototypeOf: {e:?}").into())
            })?
    };
    if !success {
        return Err(throw_type_error(
            runtime,
            "Object.setPrototypeOf could not set prototype",
        )?);
    }
    Ok(RegisterValue::from_object_handle(target.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.1  Object.assign(target, ...sources)
// ---------------------------------------------------------------------------
fn object_assign(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    for source_arg in args.iter().skip(1) {
        if *source_arg == RegisterValue::undefined() || *source_arg == RegisterValue::null() {
            continue;
        }

        let source = to_object_for_introspection(*source_arg, runtime)?;
        let keys = if runtime.is_proxy(source) {
            proxy_enumerable_own_keys(runtime, source)?
        } else {
            runtime.enumerable_own_property_keys(source).map_err(|e| {
                VmNativeCallError::Internal(format!("Object.assign keys: {e}").into())
            })?
        };

        for key_id in keys {
            let value = runtime
                .own_property_value(source, key_id)
                .map_err(|e| with_vm_context(e, "Object.assign get"))?;
            let success = runtime
                .ordinary_set(
                    target,
                    key_id,
                    RegisterValue::from_object_handle(target.0),
                    value,
                )
                .map_err(|e| with_vm_context(e, "Object.assign set"))?;
            if !success {
                let error = runtime
                    .alloc_type_error("Object.assign could not assign property")
                    .map_err(|e| {
                        VmNativeCallError::Internal(format!("Object.assign TypeError: {e}").into())
                    })?;
                return Err(VmNativeCallError::Thrown(
                    RegisterValue::from_object_handle(error.0),
                ));
            }
        }
    }

    Ok(RegisterValue::from_object_handle(target.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.2.11  Object.getOwnPropertyNames(O)
// ---------------------------------------------------------------------------
fn object_get_own_property_names(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // §10.5.11 — Proxy [[OwnPropertyKeys]] trap
    let keys = if runtime.is_proxy(target) {
        runtime.proxy_own_keys(target).map_err(interp_to_native)?
    } else {
        runtime.own_property_keys(target).map_err(|e| {
            VmNativeCallError::Internal(format!("Object.getOwnPropertyNames: {e:?}").into())
        })?
    };

    // All own string-keyed properties (no enumerable filter).
    let key_names: Vec<String> = keys
        .iter()
        .filter(|key| !runtime.property_names().is_symbol(**key))
        .map(|k| runtime.property_names().get(*k).unwrap_or("").to_string())
        .collect();

    let array = runtime.alloc_array();
    for name in &key_names {
        let str_handle = runtime.alloc_string(name.as_str());
        runtime
            .objects_mut()
            .push_element(array, RegisterValue::from_object_handle(str_handle.0))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// §20.1.2.11  Object.getOwnPropertySymbols(O)
// Spec: <https://tc39.es/ecma262/#sec-object.getownpropertysymbols>
// ---------------------------------------------------------------------------
fn object_get_own_property_symbols(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(
        args.first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined),
        runtime,
    )?;

    // §10.5.11 — Proxy [[OwnPropertyKeys]] trap
    let keys = if runtime.is_proxy(target) {
        runtime.proxy_own_keys(target).map_err(interp_to_native)?
    } else {
        runtime.own_property_keys(target).map_err(|e| {
            VmNativeCallError::Internal(format!("Object.getOwnPropertySymbols: {e:?}").into())
        })?
    };

    // Collect only symbol-keyed own properties.
    let symbol_ids: Vec<u32> = keys
        .iter()
        .filter_map(|key| runtime.property_names().symbol_id(*key))
        .collect();

    let array = runtime.alloc_array();
    for sid in &symbol_ids {
        runtime
            .objects_mut()
            .push_element(array, RegisterValue::from_symbol_id(*sid))
            .ok();
    }

    Ok(RegisterValue::from_object_handle(array.0))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.3.4  Object.prototype.propertyIsEnumerable(V)
// ---------------------------------------------------------------------------
fn object_property_is_enumerable(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;

    let has_own = runtime
        .own_property_descriptor(target, property)
        .map_err(|e| VmNativeCallError::Internal(format!("propertyIsEnumerable: {e:?}").into()))?;
    Ok(RegisterValue::from_bool(
        has_own
            .map(|descriptor| descriptor.attributes().enumerable())
            .unwrap_or(false),
    ))
}

// ---------------------------------------------------------------------------
// ES2024 §20.1.3.2  Object.prototype.hasOwnProperty(V)
// ---------------------------------------------------------------------------
fn object_has_own_property(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let target = to_object_for_introspection(*this, runtime)?;
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let property = runtime.property_name_from_value(key)?;
    let has = runtime
        .own_property_descriptor(target, property)
        .map_err(|e| VmNativeCallError::Internal(format!("hasOwnProperty: {e:?}").into()))?
        .is_some();
    Ok(RegisterValue::from_bool(has))
}

fn has_own_data_slot(
    handle: ObjectHandle,
    slot_name: &str,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<bool, VmNativeCallError> {
    let backing = runtime.intern_property_name(slot_name);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("data slot lookup failed: {error:?}").into())
        })?
    else {
        return Ok(false);
    };

    if lookup.owner() != handle {
        return Ok(false);
    }

    Ok(matches!(lookup.value(), PropertyValue::Data { .. }))
}

/// ES2024 §20.1.2.6 Object.fromEntries(iterable)
fn object_from_entries(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let iterable = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);

    let result = runtime.alloc_object();

    let Some(arr_handle) = iterable.as_object_handle().map(ObjectHandle) else {
        return Err(throw_type_error(
            runtime,
            "Object.fromEntries requires an iterable argument",
        )?);
    };

    if matches!(runtime.objects().kind(arr_handle), Ok(HeapValueKind::Array)) {
        let length = runtime
            .objects()
            .array_length(arr_handle)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?
            .unwrap_or(0);
        for index in 0..length {
            let entry = runtime.get_array_index_value(arr_handle, index)?;
            let Some(entry) = entry else { continue };
            let Some(entry_handle) = entry.as_object_handle().map(ObjectHandle) else {
                continue;
            };
            if !matches!(
                runtime.objects().kind(entry_handle),
                Ok(HeapValueKind::Array)
            ) {
                continue;
            }
            let key = runtime
                .get_array_index_value(entry_handle, 0)?
                .unwrap_or_else(RegisterValue::undefined);
            let value = runtime
                .get_array_index_value(entry_handle, 1)?
                .unwrap_or_else(RegisterValue::undefined);
            let property = runtime.property_name_from_value(key)?;
            runtime
                .objects_mut()
                .set_property(result, property, value)
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
        }
    }

    Ok(RegisterValue::from_object_handle(result.0))
}

/// ES2024 §20.1.3.5 Object.prototype.toLocaleString()
fn object_to_locale_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    // Calls this.toString().
    let handle = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("toLocaleString requires object receiver".into())
    })?;
    let to_string_prop = runtime.intern_property_name("toString");
    let to_string = runtime
        .ordinary_get(handle, to_string_prop, *this)
        .map_err(|e| match e {
            VmNativeCallError::Thrown(v) => VmNativeCallError::Thrown(v),
            other => other,
        })?;
    if let Some(callable) = to_string.as_object_handle().map(ObjectHandle)
        && runtime.objects().is_callable(callable)
    {
        return runtime.call_callable(callable, *this, &[]);
    }
    let text = runtime.js_to_string(*this).map_err(|e| match e {
        crate::interpreter::InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
        other => VmNativeCallError::Internal(format!("{other}").into()),
    })?;
    let handle = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(handle.0))
}

/// Returns enumerable own string keys for a proxy target, using proxy traps.
fn proxy_enumerable_own_keys(
    runtime: &mut crate::interpreter::RuntimeState,
    target: ObjectHandle,
) -> Result<Vec<crate::property::PropertyNameId>, VmNativeCallError> {
    let all_keys = runtime.proxy_own_keys(target).map_err(interp_to_native)?;
    let mut result = Vec::with_capacity(all_keys.len());
    for key in all_keys {
        if runtime.property_names().is_symbol(key) {
            continue;
        }
        let desc = runtime
            .proxy_get_own_property_descriptor(target, key)
            .map_err(interp_to_native)?;
        if let Some(pv) = desc
            && pv.attributes().enumerable()
        {
            result.push(key);
        }
    }
    Ok(result)
}

/// Converts an `InterpreterError` to a `VmNativeCallError` for use in intrinsic functions.
/// Must be called through `interp_to_native_with_rt` when the error might be a TypeError
/// that needs to be catchable in JS.
fn interp_to_native(e: crate::interpreter::InterpreterError) -> VmNativeCallError {
    match e {
        crate::interpreter::InterpreterError::UncaughtThrow(v) => VmNativeCallError::Thrown(v),
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}

// ---------------------------------------------------------------------------
// §22.1.2.11 Object.groupBy(items, callbackfn)
// ---------------------------------------------------------------------------

/// `Object.groupBy(items, callbackfn)` — §22.1.2.11
/// <https://tc39.es/ecma262/#sec-object.groupby>
///
/// Groups elements of an iterable into a null-prototype object whose keys are
/// the string-coerced results of `callbackfn(element, index)`.
fn object_group_by(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let items = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    let callback = args
        .get(1)
        .and_then(|v| v.as_object_handle().map(ObjectHandle))
        .filter(|h| runtime.objects().is_callable(*h))
        .ok_or_else(|| {
            VmNativeCallError::Internal("Object.groupBy: callbackfn is not a function".into())
        })?;

    let source = items.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Object.groupBy: items is not an object".into())
    })?;

    // Collect elements via array fast-path.
    let length = runtime
        .objects()
        .array_length(source)
        .ok()
        .flatten()
        .unwrap_or(0);

    // Result is a null-prototype object.
    let result = runtime.objects_mut().alloc_object();
    // null prototype (no inherited properties)

    for i in 0..length {
        let value = runtime
            .get_array_index_value(source, i)?
            .unwrap_or_else(RegisterValue::undefined);

        let key = runtime.call_callable(
            callback,
            RegisterValue::undefined(),
            &[value, RegisterValue::from_i32(i as i32)],
        )?;

        // ToPropertyKey → ToString for the group key.
        let key_str = runtime.js_to_string(key).map_err(interp_to_native)?;
        let key_prop = runtime.intern_property_name(&key_str);

        // Get or create the group array.
        let group = match runtime.objects().get_property_with_registry(
            result,
            key_prop,
            runtime.property_names(),
        ) {
            Ok(Some(lookup)) => match lookup.value() {
                PropertyValue::Data { value: v, .. } => v.as_object_handle().map(ObjectHandle),
                _ => None,
            },
            _ => None,
        };

        let group = if let Some(g) = group {
            g
        } else {
            let g = runtime.alloc_array();
            runtime
                .objects_mut()
                .set_property(result, key_prop, RegisterValue::from_object_handle(g.0))
                .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
            g
        };

        runtime
            .objects_mut()
            .push_element(group, value)
            .map_err(|e| VmNativeCallError::Internal(format!("{e:?}").into()))?;
    }

    Ok(RegisterValue::from_object_handle(result.0))
}
