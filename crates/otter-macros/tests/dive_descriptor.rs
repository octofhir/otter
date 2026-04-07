use otter_macros::dive;
use otter_vm::{
    NativeBindingTarget, NativeEntrypointKind, NativeSlotKind, RegisterValue, RuntimeState,
    VmNativeCallError,
};

#[dive(name = "double", length = 1)]
fn dive_double(
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

#[dive(name = "version", getter)]
fn dive_version(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_i32(7))
}

#[dive(name = "Thing", constructor, length = 1)]
fn dive_constructor(
    this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(*this)
}

#[dive(name = "fetchThing", deep, length = 1)]
fn dive_async_method(
    _this: &RegisterValue,
    args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(args
        .first()
        .copied()
        .unwrap_or_else(RegisterValue::undefined))
}

#[test]
fn dive_generates_active_vm_descriptors() {
    let method = dive_double_descriptor();
    assert_eq!(DIVE_DOUBLE_NAME, "double");
    assert_eq!(DIVE_DOUBLE_LENGTH, 1);
    assert_eq!(method.js_name(), "double");
    assert_eq!(method.length(), 1);
    assert_eq!(method.slot_kind(), NativeSlotKind::Method);
    assert_eq!(method.entrypoint_kind(), NativeEntrypointKind::Sync);

    let getter = dive_version_descriptor();
    assert_eq!(getter.js_name(), "version");
    assert_eq!(getter.length(), 0);
    assert_eq!(getter.slot_kind(), NativeSlotKind::Getter);

    let constructor = dive_constructor_descriptor();
    assert_eq!(constructor.js_name(), "Thing");
    assert_eq!(constructor.length(), 1);
    assert_eq!(constructor.slot_kind(), NativeSlotKind::Constructor);

    let async_method = dive_async_method_descriptor();
    assert_eq!(async_method.js_name(), "fetchThing");
    assert_eq!(async_method.entrypoint_kind(), NativeEntrypointKind::Async);
    assert_eq!(async_method.slot_kind(), NativeSlotKind::Method);
}

#[test]
fn dive_binding_wraps_descriptor_for_target_installation() {
    let binding = dive_double_binding(NativeBindingTarget::Namespace);
    assert_eq!(binding.target(), NativeBindingTarget::Namespace);
    assert_eq!(binding.function().js_name(), "double");
    assert_eq!(binding.function().slot_kind(), NativeSlotKind::Method);
}

#[test]
fn dive_descriptor_callback_invokes_original_function() {
    let descriptor = dive_double_descriptor();
    let value = (descriptor.callback())(
        &RegisterValue::undefined(),
        &[RegisterValue::from_i32(9)],
        &mut RuntimeState::default(),
    )
    .expect("callback should succeed");

    assert_eq!(value, RegisterValue::from_i32(18));
}
