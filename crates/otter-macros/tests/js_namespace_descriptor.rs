use otter_macros::{js_getter, js_method, js_namespace, js_setter};
use otter_vm::{
    JsNamespaceDescriptor, NativeBindingTarget, NativeSlotKind, RegisterValue, RuntimeState,
    VmNativeCallError,
};

#[js_namespace(name = "Tools")]
struct ToolsNamespace;

#[js_namespace]
impl ToolsNamespace {
    #[js_method(name = "double", length = 1)]
    fn double(
        _this: &RegisterValue,
        args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let value = args
            .first()
            .and_then(|value| (*value).as_i32())
            .unwrap_or_default();
        Ok(RegisterValue::from_i32(value.saturating_mul(2)))
    }

    #[js_getter(name = "version")]
    fn version(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(RegisterValue::from_i32(7))
    }

    #[js_setter(name = "version")]
    fn set_version(
        this: &RegisterValue,
        args: &[RegisterValue],
        runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let receiver = this
            .as_object_handle()
            .ok_or_else(|| VmNativeCallError::Internal("expected object receiver".into()))?;
        let value = args
            .first()
            .copied()
            .unwrap_or_else(RegisterValue::undefined);
        let property = runtime.intern_property_name("__version");
        runtime
            .objects_mut()
            .set_property(otter_vm::object::ObjectHandle(receiver), property, value)
            .map_err(|error| VmNativeCallError::Internal(format!("{error:?}").into()))?;
        Ok(RegisterValue::undefined())
    }
}

#[test]
fn js_namespace_descriptor_collects_active_runtime_metadata() {
    let descriptor: JsNamespaceDescriptor = ToolsNamespace::js_namespace_descriptor();

    assert_eq!(ToolsNamespace::JS_NAMESPACE_NAME, "Tools");
    assert_eq!(descriptor.js_name(), "Tools");
    assert_eq!(descriptor.bindings().len(), 3);

    let double = &descriptor.bindings()[0];
    assert_eq!(double.target(), NativeBindingTarget::Namespace);
    assert_eq!(double.function().js_name(), "double");
    assert_eq!(double.function().slot_kind(), NativeSlotKind::Method);
    assert_eq!(double.function().length(), 1);

    let version_get = &descriptor.bindings()[1];
    assert_eq!(version_get.target(), NativeBindingTarget::Namespace);
    assert_eq!(version_get.function().js_name(), "version");
    assert_eq!(version_get.function().slot_kind(), NativeSlotKind::Getter);

    let version_set = &descriptor.bindings()[2];
    assert_eq!(version_set.target(), NativeBindingTarget::Namespace);
    assert_eq!(version_set.function().js_name(), "version");
    assert_eq!(version_set.function().slot_kind(), NativeSlotKind::Setter);
    assert_eq!(version_set.function().length(), 1);
}

#[test]
fn js_namespace_member_descriptor_invokes_native_callback() {
    let descriptor = ToolsNamespace::double_descriptor();
    let value = (descriptor.callback())(
        &RegisterValue::undefined(),
        &[RegisterValue::from_i32(6)],
        &mut RuntimeState::default(),
    )
    .expect("callback should succeed");

    assert_eq!(value, RegisterValue::from_i32(12));
}
