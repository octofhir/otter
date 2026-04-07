use otter_macros::{burrow, dive};
use otter_vm::{NativeSlotKind, RegisterValue, RuntimeState, VmNativeCallError};

#[dive(name = "size", getter)]
fn size(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    _runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(RegisterValue::from_i32(1))
}

#[dive(name = "set", length = 2)]
fn set(
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
fn burrow_groups_dive_descriptors_for_host_object_surfaces() {
    let descriptors = burrow! {
        fns = [set, size]
    };

    assert_eq!(descriptors.len(), 2);

    let set = &descriptors[0];
    assert_eq!(set.js_name(), "set");
    assert_eq!(set.length(), 2);
    assert_eq!(set.slot_kind(), NativeSlotKind::Method);

    let size = &descriptors[1];
    assert_eq!(size.js_name(), "size");
    assert_eq!(size.slot_kind(), NativeSlotKind::Getter);
}
