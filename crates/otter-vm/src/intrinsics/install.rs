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

    pub(super) fn alloc_intrinsic_object(
        &mut self,
        prototype: Option<ObjectHandle>,
    ) -> Result<ObjectHandle, IntrinsicsError> {
        let handle = self.heap.alloc_object();
        self.heap.set_prototype(handle, prototype)?;
        Ok(handle)
    }

    pub(super) fn alloc_intrinsic_host_function(
        &mut self,
        function: crate::host::HostFunctionId,
        prototype: ObjectHandle,
    ) -> Result<ObjectHandle, IntrinsicsError> {
        let handle = self.heap.alloc_host_function(function);
        self.heap.set_prototype(handle, Some(prototype))?;
        Ok(handle)
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
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    for member in plan.members() {
        match member {
            ObjectMemberPlan::Method(function) => {
                let host_function = cx.native_functions.register(function.clone());
                let handle = cx.alloc_intrinsic_host_function(host_function, function_prototype)?;
                let property = cx.property_names.intern(function.js_name());
                cx.heap.set_property(
                    target,
                    property,
                    RegisterValue::from_object_handle(handle.0),
                )?;
            }
            ObjectMemberPlan::Accessor(accessor) => {
                let getter = accessor
                    .getter()
                    .cloned()
                    .map(|descriptor| {
                        let host_function = cx.native_functions.register(descriptor);
                        cx.alloc_intrinsic_host_function(host_function, function_prototype)
                    })
                    .transpose()?;
                let setter = accessor
                    .setter()
                    .cloned()
                    .map(|descriptor| {
                        let host_function = cx.native_functions.register(descriptor);
                        cx.alloc_intrinsic_host_function(host_function, function_prototype)
                    })
                    .transpose()?;
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
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    install_class_members(prototype, plan.prototype_members(), function_prototype, cx)?;
    install_class_members(constructor, plan.static_members(), function_prototype, cx)?;

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
    function_prototype: ObjectHandle,
    cx: &mut IntrinsicInstallContext<'_>,
) -> Result<(), IntrinsicsError> {
    for member in members {
        match member {
            ClassMemberPlan::Method(function) => {
                let host_function = cx.native_functions.register(function.clone());
                let handle = cx.alloc_intrinsic_host_function(host_function, function_prototype)?;
                let property = cx.property_names.intern(function.js_name());
                cx.heap.set_property(
                    target,
                    property,
                    RegisterValue::from_object_handle(handle.0),
                )?;
            }
            ClassMemberPlan::Accessor(accessor) => {
                let getter = accessor
                    .getter()
                    .cloned()
                    .map(|descriptor| {
                        let host_function = cx.native_functions.register(descriptor);
                        cx.alloc_intrinsic_host_function(host_function, function_prototype)
                    })
                    .transpose()?;
                let setter = accessor
                    .setter()
                    .cloned()
                    .map(|descriptor| {
                        let host_function = cx.native_functions.register(descriptor);
                        cx.alloc_intrinsic_host_function(host_function, function_prototype)
                    })
                    .transpose()?;
                let property = cx.property_names.intern(accessor.js_name());
                cx.heap.define_accessor(target, property, getter, setter)?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use crate::builders::NamespaceBuilder;
    use crate::descriptors::{
        JsNamespaceDescriptor, NativeBindingDescriptor, NativeBindingTarget,
        NativeFunctionDescriptor, VmNativeCallError,
    };
    use crate::host::NativeFunctionRegistry;
    use crate::intrinsics::VmIntrinsics;
    use crate::object::{HeapValueKind, ObjectHeap, PropertyValue};
    use crate::property::PropertyNameRegistry;
    use crate::value::RegisterValue;

    use super::{IntrinsicInstallContext, install_object_plan};

    fn namespace_double(
        _this: &RegisterValue,
        args: &[RegisterValue],
        _runtime: &mut crate::interpreter::RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let value = args
            .first()
            .and_then(|value| (*value).as_i32())
            .unwrap_or_default();
        Ok(RegisterValue::from_i32(value.saturating_mul(2)))
    }

    #[test]
    fn namespace_descriptor_installs_through_intrinsic_runtime_path() {
        let descriptor =
            JsNamespaceDescriptor::new("Tools").with_binding(NativeBindingDescriptor::new(
                NativeBindingTarget::Namespace,
                NativeFunctionDescriptor::method("double", 1, namespace_double),
            ));
        let plan = NamespaceBuilder::from_bindings(descriptor.bindings())
            .expect("namespace descriptor should normalize")
            .build();

        let mut heap = ObjectHeap::new();
        let mut intrinsics = VmIntrinsics::allocate(&mut heap);
        let mut property_names = PropertyNameRegistry::new();
        let mut native_functions = NativeFunctionRegistry::new();
        intrinsics
            .wire_prototype_chains(&mut heap)
            .expect("intrinsic prototype wiring should succeed");

        let namespace = {
            let mut cx =
                IntrinsicInstallContext::new(&mut heap, &mut property_names, &mut native_functions);
            let namespace = cx
                .alloc_intrinsic_object(Some(intrinsics.object_prototype()))
                .expect("namespace object should allocate");
            install_object_plan(namespace, &plan, intrinsics.function_prototype(), &mut cx)
                .expect("namespace plan should install");
            cx.install_global_value(
                &intrinsics,
                descriptor.js_name(),
                RegisterValue::from_object_handle(namespace.0),
            )
            .expect("namespace should install on global");
            namespace
        };

        let tools = property_names.intern("Tools");
        let double = property_names.intern("double");
        let global_lookup = heap
            .get_property(intrinsics.global_object(), tools)
            .expect("global lookup should succeed")
            .expect("namespace should be installed");
        let PropertyValue::Data(global_value) = global_lookup.value() else {
            panic!("namespace should install as a data property");
        };
        assert_eq!(global_value, RegisterValue::from_object_handle(namespace.0));
        assert_eq!(heap.kind(namespace), Ok(HeapValueKind::Object));

        let method_lookup = heap
            .get_property(namespace, double)
            .expect("method lookup should succeed")
            .expect("namespace method should install");
        let PropertyValue::Data(method) = method_lookup.value() else {
            panic!("namespace method should be a data property");
        };
        let method = method
            .as_object_handle()
            .map(crate::object::ObjectHandle)
            .expect("method value should be a callable object");
        assert_eq!(heap.kind(method), Ok(HeapValueKind::HostFunction));
        assert_eq!(native_functions.len(), 1);
    }
}
