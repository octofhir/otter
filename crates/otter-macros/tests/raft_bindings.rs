use otter_macros::{dive, raft};
use otter_vm::{
    NativeBindingTarget, NativeSlotKind, RegisterValue, RuntimeState, VmNativeCallError,
};

#[dive(name = "alpha", length = 0)]
fn alpha(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_i32(1))
}

#[dive(name = "beta", length = 1)]
fn beta(
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
fn raft_groups_dive_bindings_for_one_target() {
    let bindings = raft! {
        target = Namespace,
        fns = [alpha, beta]
    };

    assert_eq!(bindings.len(), 2);

    let alpha = &bindings[0];
    assert_eq!(alpha.target(), NativeBindingTarget::Namespace);
    assert_eq!(alpha.function().js_name(), "alpha");
    assert_eq!(alpha.function().slot_kind(), NativeSlotKind::Method);

    let beta = &bindings[1];
    assert_eq!(beta.target(), NativeBindingTarget::Namespace);
    assert_eq!(beta.function().js_name(), "beta");
    assert_eq!(beta.function().length(), 1);
}
