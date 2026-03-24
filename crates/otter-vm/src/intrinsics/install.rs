use crate::builders::{ClassInstallPlan, ClassMemberPlan, ObjectInstallPlan, ObjectMemberPlan};
use crate::host::NativeFunctionRegistry;
use crate::object::{ObjectHandle, ObjectHeap};
use crate::property::PropertyNameRegistry;
use crate::value::RegisterValue;

use super::{IntrinsicsError, VmIntrinsics};

pub(super) struct IntrinsicInstallContext<'a> {
    pub(super) heap: &'a mut ObjectHeap,
    pub(super) property_names: &'a mut PropertyNameRegistry,
    pub(super) native_functions: &'a mut NativeFunctionRegistry,
}

impl<'a> IntrinsicInstallContext<'a> {
    pub(super) fn new(
        heap: &'a mut ObjectHeap,
        property_names: &'a mut PropertyNameRegistry,
        native_functions: &'a mut NativeFunctionRegistry,
    ) -> Self {
        Self {
            heap,
            property_names,
            native_functions,
        }
    }

    pub(super) fn install_global_value(
        &mut self,
        intrinsics: &VmIntrinsics,
        js_name: &str,
        value: RegisterValue,
    ) -> Result<(), IntrinsicsError> {
        let property = self.property_names.intern(js_name);
        self.heap
            .set_property(intrinsics.global_object(), property, value)?;
        Ok(())
    }
}

pub(super) trait IntrinsicInstaller {
    fn init(
        &self,
        intrinsics: &mut VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError>;

    fn install_on_global(
        &self,
        intrinsics: &VmIntrinsics,
        cx: &mut IntrinsicInstallContext<'_>,
    ) -> Result<(), IntrinsicsError>;
}

pub(super) fn install_object_plan(
    target: ObjectHandle,
    plan: &ObjectInstallPlan,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    for member in plan.members() {
        match member {
            ObjectMemberPlan::Method(function) => {
                let host_function = cx.native_functions.register(function.clone());
                let handle = cx.heap.alloc_host_function(host_function);
                let property = cx.property_names.intern(function.js_name());
                cx.heap.set_property(
                    target,
                    property,
                    RegisterValue::from_object_handle(handle.0),
                )?;
            }
            ObjectMemberPlan::Accessor(accessor) => {
                let getter = accessor.getter().cloned().map(|descriptor| {
                    let host_function = cx.native_functions.register(descriptor);
                    cx.heap.alloc_host_function(host_function)
                });
                let setter = accessor.setter().cloned().map(|descriptor| {
                    let host_function = cx.native_functions.register(descriptor);
                    cx.heap.alloc_host_function(host_function)
                });
                let property = cx.property_names.intern(accessor.js_name());
                cx.heap.define_accessor(target, property, getter, setter)?;
            }
        }
    }

    Ok(())
}

pub(super) fn install_class_plan(
    prototype: ObjectHandle,
    constructor: ObjectHandle,
    plan: &ClassInstallPlan,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    install_class_members(prototype, plan.prototype_members(), cx)?;
    install_class_members(constructor, plan.static_members(), cx)?;

    let prototype_property = cx.property_names.intern("prototype");
    cx.heap.set_property(
        constructor,
        prototype_property,
        RegisterValue::from_object_handle(prototype.0),
    )?;

    let constructor_property = cx.property_names.intern("constructor");
    cx.heap.set_property(
        prototype,
        constructor_property,
        RegisterValue::from_object_handle(constructor.0),
    )?;

    Ok(())
}

fn install_class_members(
    target: ObjectHandle,
    members: &[ClassMemberPlan],
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    for member in members {
        match member {
            ClassMemberPlan::Method(function) => {
                let host_function = cx.native_functions.register(function.clone());
                let handle = cx.heap.alloc_host_function(host_function);
                let property = cx.property_names.intern(function.js_name());
                cx.heap.set_property(
                    target,
                    property,
                    RegisterValue::from_object_handle(handle.0),
                )?;
            }
            ClassMemberPlan::Accessor(accessor) => {
                let getter = accessor.getter().cloned().map(|descriptor| {
                    let host_function = cx.native_functions.register(descriptor);
                    cx.heap.alloc_host_function(host_function)
                });
                let setter = accessor.setter().cloned().map(|descriptor| {
                    let host_function = cx.native_functions.register(descriptor);
                    cx.heap.alloc_host_function(host_function)
                });
                let property = cx.property_names.intern(accessor.js_name());
                cx.heap.define_accessor(target, property, getter, setter)?;
            }
        }
    }

    Ok(())
}
