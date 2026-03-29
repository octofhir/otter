use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_function_length_name},
};

pub(super) static SYMBOL_INTRINSIC: SymbolIntrinsic = SymbolIntrinsic;

pub(super) struct SymbolIntrinsic;

const SYMBOL_DATA_SLOT: &str = "__otter_symbol_data__";

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

impl IntrinsicInstaller for SymbolIntrinsic {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        let constructor_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::method(
                "Symbol",
                0,
                symbol_function,
            ));
        let constructor =
            cx.alloc_intrinsic_host_function(constructor_id, intrinsics.function_prototype())?;
        install_function_length_name(constructor, 0, "Symbol", cx)?;

        let prototype_property = cx.property_names.intern("prototype");
        cx.heap.define_own_property(
            constructor,
            prototype_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(intrinsics.symbol_prototype().0),
                PropertyAttributes::frozen(),
            ),
        )?;

        let constructor_property = cx.property_names.intern("constructor");
        cx.heap.define_own_property(
            intrinsics.symbol_prototype(),
            constructor_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(constructor.0),
                PropertyAttributes::constructor_link(),
            ),
        )?;

        let key_for_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::method(
                "keyFor",
                1,
                symbol_key_for,
            ));
        let key_for =
            cx.alloc_intrinsic_host_function(key_for_id, intrinsics.function_prototype())?;
        install_function_length_name(key_for, 1, "keyFor", cx)?;
        let key_for_property = cx.property_names.intern("keyFor");
        cx.heap.define_own_property(
            constructor,
            key_for_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(key_for.0),
                PropertyAttributes::builtin_method(),
            ),
        )?;

        let symbol_for_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::method("for", 1, symbol_for));
        let symbol_for =
            cx.alloc_intrinsic_host_function(symbol_for_id, intrinsics.function_prototype())?;
        install_function_length_name(symbol_for, 1, "for", cx)?;
        let for_property = cx.property_names.intern("for");
        cx.heap.define_own_property(
            constructor,
            for_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(symbol_for.0),
                PropertyAttributes::builtin_method(),
            ),
        )?;

        let value_of_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::method(
                "valueOf",
                0,
                symbol_prototype_value_of,
            ));
        let value_of =
            cx.alloc_intrinsic_host_function(value_of_id, intrinsics.function_prototype())?;
        install_function_length_name(value_of, 0, "valueOf", cx)?;
        let value_of_property = cx.property_names.intern("valueOf");
        cx.heap.define_own_property(
            intrinsics.symbol_prototype(),
            value_of_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(value_of.0),
                PropertyAttributes::builtin_method(),
            ),
        )?;

        let to_string_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::method(
                "toString",
                0,
                symbol_prototype_to_string,
            ));
        let to_string =
            cx.alloc_intrinsic_host_function(to_string_id, intrinsics.function_prototype())?;
        install_function_length_name(to_string, 0, "toString", cx)?;
        let to_string_property = cx.property_names.intern("toString");
        cx.heap.define_own_property(
            intrinsics.symbol_prototype(),
            to_string_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(to_string.0),
                PropertyAttributes::builtin_method(),
            ),
        )?;

        let to_primitive_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::method(
                "[Symbol.toPrimitive]",
                1,
                symbol_prototype_to_primitive,
            ));
        let to_primitive =
            cx.alloc_intrinsic_host_function(to_primitive_id, intrinsics.function_prototype())?;
        install_function_length_name(to_primitive, 1, "[Symbol.toPrimitive]", cx)?;
        let to_primitive_property = cx
            .property_names
            .intern_symbol(super::WellKnownSymbol::ToPrimitive.stable_id());
        cx.heap.define_own_property(
            intrinsics.symbol_prototype(),
            to_primitive_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(to_primitive.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        let description_id = cx
            .native_functions
            .register(NativeFunctionDescriptor::getter(
                "get description",
                symbol_prototype_description_getter,
            ));
        let description_getter =
            cx.alloc_intrinsic_host_function(description_id, intrinsics.function_prototype())?;
        install_function_length_name(description_getter, 0, "get description", cx)?;
        let description_property = cx.property_names.intern("description");
        cx.heap.define_accessor(
            intrinsics.symbol_prototype(),
            description_property,
            Some(description_getter),
            None,
        )?;

        let to_string_tag_property = cx
            .property_names
            .intern_symbol(super::WellKnownSymbol::ToStringTag.stable_id());
        let tag_value = cx.heap.alloc_string("Symbol");
        cx.heap.define_own_property(
            intrinsics.symbol_prototype(),
            to_string_tag_property,
            PropertyValue::data_with_attrs(
                RegisterValue::from_object_handle(tag_value.0),
                PropertyAttributes::from_flags(false, false, true),
            ),
        )?;

        for &symbol in intrinsics.well_known_symbols() {
            let property_name = symbol
                .description()
                .strip_prefix("Symbol.")
                .expect("well-known symbol descriptions use Symbol.<name>");
            let property = cx.property_names.intern(property_name);
            cx.heap.define_own_property(
                constructor,
                property,
                PropertyValue::data_with_attrs(
                    intrinsics.well_known_symbol_value(symbol),
                    PropertyAttributes::constant(),
                ),
            )?;
        }

        intrinsics.symbol_constructor = constructor;
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        cx.install_global_value(
            intrinsics,
            "Symbol",
            RegisterValue::from_object_handle(intrinsics.symbol_constructor().0),
        )
    }
}

fn symbol_function(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let description = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .create_symbol_from_value(description)
        .map_err(|error| map_interpreter_error(error, runtime))
}

fn symbol_for(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let key = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    runtime
        .symbol_for_value(key)
        .map_err(|error| map_interpreter_error(error, runtime))
}

fn symbol_key_for(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined);
    if !value.is_symbol() {
        return Err(type_error(
            runtime,
            "Symbol.keyFor requires a symbol argument",
        )?);
    }
    let Some(key) = runtime.symbol_registry_key(value).map(str::to_owned) else {
        return Ok(RegisterValue::undefined());
    };
    let key = runtime.alloc_string(key);
    Ok(RegisterValue::from_object_handle(key.0))
}

fn symbol_prototype_value_of(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    this_symbol_value(*this, runtime)
}

fn symbol_prototype_to_string(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let symbol = this_symbol_value(*this, runtime)?;
    let text = symbol_descriptive_string(symbol, runtime);
    let text = runtime.alloc_string(text);
    Ok(RegisterValue::from_object_handle(text.0))
}

fn symbol_prototype_to_primitive(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    this_symbol_value(*this, runtime)
}

fn symbol_prototype_description_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let symbol = this_symbol_value(*this, runtime)?;
    let Some(description) = runtime.symbol_description(symbol).map(str::to_owned) else {
        return Ok(RegisterValue::undefined());
    };
    let description = runtime.alloc_string(description);
    Ok(RegisterValue::from_object_handle(description.0))
}

fn this_symbol_value(
    this: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if this.is_symbol() {
        return Ok(this);
    }
    let Some(handle) = this.as_object_handle().map(ObjectHandle) else {
        return Err(type_error(
            runtime,
            "Symbol.prototype method requires that 'this' be a Symbol",
        )?);
    };
    let Some(symbol) = symbol_data(handle, runtime)? else {
        return Err(type_error(
            runtime,
            "Symbol.prototype method requires that 'this' be a Symbol",
        )?);
    };
    Ok(symbol)
}

pub(crate) fn box_symbol_object(
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let wrapper =
        runtime.alloc_object_with_prototype(Some(runtime.intrinsics().symbol_prototype()));
    set_symbol_data(wrapper, primitive, runtime)?;
    Ok(RegisterValue::from_object_handle(wrapper.0))
}

fn set_symbol_data(
    receiver: ObjectHandle,
    primitive: RegisterValue,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<(), VmNativeCallError> {
    let backing = runtime.intern_property_name(SYMBOL_DATA_SLOT);
    runtime
        .objects_mut()
        .define_own_property(
            receiver,
            backing,
            PropertyValue::data_with_attrs(
                primitive,
                PropertyAttributes::from_flags(true, false, true),
            ),
        )
        .map_err(|error| {
            VmNativeCallError::Internal(
                format!("Symbol constructor backing store failed: {error:?}").into(),
            )
        })?;
    Ok(())
}

fn symbol_data(
    handle: ObjectHandle,
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<Option<RegisterValue>, VmNativeCallError> {
    let backing = runtime.intern_property_name(SYMBOL_DATA_SLOT);
    let Some(lookup) = runtime
        .objects()
        .get_property(handle, backing)
        .map_err(|error| {
            VmNativeCallError::Internal(format!("Symbol data lookup failed: {error:?}").into())
        })?
    else {
        return Ok(None);
    };
    let PropertyValue::Data { value, .. } = lookup.value() else {
        return Ok(None);
    };
    Ok(Some(value))
}

pub(crate) fn symbol_descriptive_string(
    value: RegisterValue,
    runtime: &crate::interpreter::RuntimeState,
) -> String {
    let description = runtime.symbol_description(value).unwrap_or("");
    format!("Symbol({description})")
}

fn map_interpreter_error(
    error: crate::interpreter::InterpreterError,
    runtime: &mut crate::interpreter::RuntimeState,
) -> VmNativeCallError {
    match error {
        crate::interpreter::InterpreterError::UncaughtThrow(value) => {
            VmNativeCallError::Thrown(value)
        }
        crate::interpreter::InterpreterError::TypeError(message) => match type_error(runtime, &message)
        {
            Ok(error) => error,
            Err(error) => error,
        }
        crate::interpreter::InterpreterError::NativeCall(message) => {
            VmNativeCallError::Internal(message)
        }
        other => VmNativeCallError::Internal(format!("{other}").into()),
    }
}
