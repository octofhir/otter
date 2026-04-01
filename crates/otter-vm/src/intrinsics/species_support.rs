use crate::descriptors::{NativeFunctionDescriptor, VmNativeCallError};
use crate::object::ObjectHandle;
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
        install_species_getter(
            cx,
            intrinsics.array_constructor(),
            intrinsics.function_prototype(),
        )?;
        install_species_getter(
            cx,
            intrinsics.promise_constructor(),
            intrinsics.function_prototype(),
        )?;

        install_species_getter(
            cx,
            intrinsics.regexp_constructor(),
            intrinsics.function_prototype(),
        )?;
        Ok(())
    }
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
