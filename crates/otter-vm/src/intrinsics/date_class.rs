use std::time::{SystemTime, UNIX_EPOCH};

use crate::builders::ClassBuilder;
use crate::descriptors::{
    JsClassDescriptor, NativeBindingDescriptor, NativeBindingTarget, NativeFunctionDescriptor,
    VmNativeCallError,
};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_class_plan},
};

pub(super) static DATE_INTRINSIC: DateIntrinsic = DateIntrinsic;

const DATE_DATA_SLOT: &str = "__otter_date_data__";

pub(super) struct DateIntrinsic;

impl IntrinsicInstaller for DateIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let descriptor = date_class_descriptor();
        let plan = ClassBuilder::from_descriptor(&descriptor)
            .expect("Date class descriptors should normalize")
            .build();

        let constructor = if let Some(descriptor) = plan.constructor() {
            let host_function = cx.native_functions.register(descriptor.clone());
            cx.alloc_intrinsic_host_function(host_function, intrinsics.function_prototype())?
        } else {
            cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?
        };

        intrinsics.date_constructor = constructor;
        install_class_plan(
            intrinsics.date_prototype(),
            intrinsics.date_constructor(),
            &plan,
            intrinsics.function_prototype(),
            cx,
        )?;

        let to_string_tag = cx
            .property_names
            .intern_symbol(WellKnownSymbol::ToStringTag.stable_id());
        let tag = cx.heap.alloc_string("Date");
        cx.heap.define_own_property(
            intrinsics.date_prototype(),
            to_string_tag,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(tag.0),
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
            "Date",
            RegisterValue::from_object_handle(intrinsics.date_constructor().0),
        )
    }
}

fn date_class_descriptor() -> JsClassDescriptor {
    JsClassDescriptor::new("Date")
        .with_constructor(NativeFunctionDescriptor::constructor(
            "Date",
            7,
            date_constructor,
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Constructor,
            NativeFunctionDescriptor::method("now", 0, date_now),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("getTime", 0, date_prototype_get_time),
        ))
        .with_binding(NativeBindingDescriptor::new(
            NativeBindingTarget::Prototype,
            NativeFunctionDescriptor::method("valueOf", 0, date_prototype_get_time),
        ))
}

fn date_constructor(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if !runtime.is_current_native_construct_call() {
        let text = runtime.alloc_string(current_time_millis().to_string());
        return Ok(RegisterValue::from_object_handle(text.0));
    }

    let receiver = this.as_object_handle().map(ObjectHandle).ok_or_else(|| {
        VmNativeCallError::Internal("Date constructor is missing a construct receiver".into())
    })?;
    let timestamp = match args {
        [] => current_time_millis(),
        [value] => date_argument_to_timestamp(*value, runtime)?,
        _ => f64::NAN,
    };
    set_date_data(receiver, timestamp, runtime)?;
    Ok(*this)
}

fn date_now(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_number(current_time_millis()))
}

fn date_prototype_get_time(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_number(date_data(*this, runtime)?))
}

fn date_argument_to_timestamp(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    if let Some(handle) = value.as_object_handle().map(ObjectHandle)
        && let Ok(timestamp) = date_data(RegisterValue::from_object_handle(handle.0), runtime)
    {
        return Ok(timestamp);
    }

    match runtime.js_to_number(value) {
        Ok(number) => Ok(number),
        Err(crate::interpreter::InterpreterError::UncaughtThrow(value)) => {
            Err(VmNativeCallError::Thrown(value))
        }
        Err(crate::interpreter::InterpreterError::TypeError(message)) => {
            let error = runtime.alloc_type_error(&message).map_err(|alloc_error| {
                VmNativeCallError::Internal(
                    format!("TypeError allocation failed: {alloc_error}").into(),
                )
            })?;
            Err(VmNativeCallError::Thrown(RegisterValue::from_object_handle(
                error.0,
            )))
        }
        Err(other) => Err(VmNativeCallError::Internal(format!("{other}").into())),
    }
}

fn current_time_millis() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as f64)
        .unwrap_or(f64::NAN)
}

fn date_data(
    value: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<f64, VmNativeCallError> {
    let Some(handle) = value.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(runtime, "Date.prototype.getTime requires a Date receiver")?);
    };
    let backing = runtime.intern_property_name(DATE_DATA_SLOT);
    let Some(lookup) = runtime.objects().get_property(handle, backing).map_err(|error| {
        VmNativeCallError::Internal(format!("Date data lookup failed: {error:?}").into())
    })? else {
        return Err(type_error(runtime, "Date.prototype.getTime requires a Date receiver")?);
    };
    let crate::object::PropertyValue::Data { value, .. } = lookup.value() else {
        return Err(type_error(runtime, "Date.prototype.getTime requires a Date receiver")?);
    };
    Ok(value.as_number().unwrap_or(f64::NAN))
}

fn set_date_data(
    receiver: ObjectHandle,
    timestamp: f64,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let backing = runtime.intern_property_name(DATE_DATA_SLOT);
    runtime
        .objects_mut()
        .define_own_property(
            receiver,
            backing,
            PropertyValue::data_with_attrs(
                RegisterValue::from_number(timestamp),
                PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(format!("Date constructor backing store failed: {error:?}").into())
        })?;
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
