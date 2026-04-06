use otter_macros::{js_class, js_constructor, js_getter, js_method, js_namespace, js_static};
use otter_vm::{
    JsNamespaceDescriptor, NativeBindingTarget, NativeSlotKind, RegisterValue, RuntimeState,
    VmNativeCallError,
};

#[js_class(name = "Counter")]
struct Counter;

#[js_class]
impl Counter {
    #[js_constructor(name = "Counter", length = 1)]
    fn constructor(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(*this)
    }

    #[js_method(name = "inc", length = 1)]
    fn inc(
        this: &RegisterValue,
        args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        let increment = args
            .first()
            .and_then(|value| (*value).as_i32())
            .unwrap_or_default();
        this.add_i32(RegisterValue::from_i32(increment))
            .map_err(|err| VmNativeCallError::Internal(err.to_string().into()))
    }

    #[js_getter(name = "value")]
    fn value(
        this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(*this)
    }

    #[js_static(name = "zero", length = 0)]
    fn zero(
        _this: &RegisterValue,
        _args: &[RegisterValue],
        _runtime: &mut RuntimeState,
    ) -> Result<RegisterValue, VmNativeCallError> {
        Ok(RegisterValue::from_i32(0))
    }
}

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
}

#[test]
fn js_class_macros_are_available_from_otter_vm_tests() {
    let descriptor = Counter::js_class_descriptor();

    assert_eq!(Counter::JS_CLASS_NAME, "Counter");
    assert_eq!(descriptor.js_name(), "Counter");

    let constructor = descriptor.constructor().expect("constructor should exist");
    assert_eq!(constructor.js_name(), "Counter");
    assert_eq!(constructor.slot_kind(), NativeSlotKind::Constructor);
    assert_eq!(constructor.length(), 1);

    assert_eq!(descriptor.bindings().len(), 3);

    let inc = &descriptor.bindings()[0];
    assert_eq!(inc.target(), NativeBindingTarget::Prototype);
    assert_eq!(inc.function().js_name(), "inc");
    assert_eq!(inc.function().slot_kind(), NativeSlotKind::Method);

    let value = &descriptor.bindings()[1];
    assert_eq!(value.target(), NativeBindingTarget::Prototype);
    assert_eq!(value.function().js_name(), "value");
    assert_eq!(value.function().slot_kind(), NativeSlotKind::Getter);

    let zero = &descriptor.bindings()[2];
    assert_eq!(zero.target(), NativeBindingTarget::Constructor);
    assert_eq!(zero.function().js_name(), "zero");
    assert_eq!(zero.function().slot_kind(), NativeSlotKind::Method);
}

#[test]
fn js_namespace_macros_are_available_from_otter_vm_tests() {
    let descriptor: JsNamespaceDescriptor = ToolsNamespace::js_namespace_descriptor();

    assert_eq!(ToolsNamespace::JS_NAMESPACE_NAME, "Tools");
    assert_eq!(descriptor.js_name(), "Tools");
    assert_eq!(descriptor.bindings().len(), 2);

    let double = &descriptor.bindings()[0];
    assert_eq!(double.target(), NativeBindingTarget::Namespace);
    assert_eq!(double.function().js_name(), "double");
    assert_eq!(double.function().slot_kind(), NativeSlotKind::Method);

    let version = &descriptor.bindings()[1];
    assert_eq!(version.target(), NativeBindingTarget::Namespace);
    assert_eq!(version.function().js_name(), "version");
    assert_eq!(version.function().slot_kind(), NativeSlotKind::Getter);
}
