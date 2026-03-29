use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::{ObjectHandle, PropertyAttributes, PropertyValue};
use crate::value::RegisterValue;

use super::{
    IntrinsicsError, VmIntrinsics, WellKnownSymbol,
    install::{IntrinsicInstallContext, IntrinsicInstaller, install_function_length_name},
};

pub(super) static SPECIES_SUPPORT_INTRINSIC: SpeciesSupportIntrinsic = SpeciesSupportIntrinsic;

pub(super) struct SpeciesSupportIntrinsic;

impl IntrinsicInstaller for SpeciesSupportIntrinsic {
    fn init(
        &self,
        _intrinsics: &mut VmIntrinsics,
        _cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        Ok(())
    }

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError> {
        install_species_getter(cx, intrinsics.array_constructor(), intrinsics.function_prototype())?;
        install_species_getter(
            cx,
            intrinsics.promise_constructor(),
            intrinsics.function_prototype(),
        )?;

        install_stub_constructor(cx, intrinsics, "RegExp", 2)?;
        install_stub_constructor(cx, intrinsics, "Map", 0)?;
        install_stub_constructor(cx, intrinsics, "Set", 0)?;
        Ok(())
    }
}

fn install_stub_constructor(
    cx: &mut IntrinsicInstallContext<'_>,
    intrinsics: &VmIntrinsics,
    name: &str,
    length: u16,
) -> Result<(), IntrinsicsError> {
    let constructor_id = cx
        .native_functions
        .register(NativeFunctionDescriptor::constructor(
            name,
            length,
            stub_constructor,
        ));
    let constructor =
        cx.alloc_intrinsic_host_function(constructor_id, intrinsics.function_prototype())?;
    install_function_length_name(constructor, length, name, cx)?;

    let prototype = cx.alloc_intrinsic_object(Some(intrinsics.object_prototype()))?;
    let prototype_property = cx.property_names.intern("prototype");
    cx.heap.define_own_property(
        constructor,
        prototype_property,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(prototype.0),
            PropertyAttributes::frozen(),
        ),
    )?;

    let constructor_property = cx.property_names.intern("constructor");
    cx.heap.define_own_property(
        prototype,
        constructor_property,
        PropertyValue::data_with_attrs(
            RegisterValue::from_object_handle(constructor.0),
            PropertyAttributes::constructor_link(),
        ),
    )?;

    install_species_getter(cx, constructor, intrinsics.function_prototype())?;
    cx.install_global_value(intrinsics, name, RegisterValue::from_object_handle(constructor.0))
}

fn install_species_getter(
    cx: &mut IntrinsicInstallContext<'_>,
    constructor: ObjectHandle,
    function_prototype: ObjectHandle,
) -> Result<(), IntrinsicsError> {
    let getter_id = cx
        .native_functions
        .register(NativeFunctionDescriptor::getter(
            "get [Symbol.species]",
            species_getter,
        ));
    let getter = cx.alloc_intrinsic_host_function(getter_id, function_prototype)?;
    install_function_length_name(getter, 0, "get [Symbol.species]", cx)?;
    let species_property = cx
        .property_names
        .intern_symbol(WellKnownSymbol::Species.stable_id());
    cx.heap
        .define_accessor(constructor, species_property, Some(getter), None)?;
    Ok(())
}

fn species_getter(
    this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(*this)
}

fn stub_constructor(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut crate::interpreter::RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    if this.as_object_handle().is_some() {
        return Ok(*this);
    }
    let object = runtime.alloc_object();
    Ok(RegisterValue::from_object_handle(object.0))
}
