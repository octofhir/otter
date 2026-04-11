use otter_macros::lodge;
use otter_runtime::{ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError};

use crate::support::{install_method, type_error};

const NODE_TEST_SLOT: &str = "__otter_node_test_export";

lodge!(
    node_test_module,
    module_specifiers = ["node:test"],
    kind = commonjs,
    default = value(node_test_export_value(runtime)?),
);

fn node_test_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, NODE_TEST_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    for name in ["test", "it", "describe", "suite"] {
        install_method(
            runtime,
            export,
            name,
            2,
            node_test_entry,
            &format!("node:test.{name}"),
        )?;
    }
    runtime.install_global_value(NODE_TEST_SLOT, RegisterValue::from_object_handle(export.0));
    Ok(RegisterValue::from_object_handle(export.0))
}

fn node_test_entry(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let callback = args
        .iter()
        .rev()
        .copied()
        .find_map(|value| value.as_object_handle().map(ObjectHandle))
        .ok_or_else(|| type_error(runtime, "node:test requires a callback"))?;
    if !runtime.objects().is_callable(callback) {
        return Err(type_error(runtime, "node:test callback must be callable"));
    }
    runtime.call_callable(callback, RegisterValue::undefined(), &[])?;
    Ok(RegisterValue::undefined())
}

fn read_global_slot(runtime: &mut RuntimeState, slot: &str) -> Result<RegisterValue, String> {
    let global = runtime.intrinsics().global_object();
    let property = runtime.intern_property_name(slot);
    let value = runtime
        .own_property_value(global, property)
        .map_err(|error| format!("failed to read global slot '{slot}': {error:?}"))?;
    if value == RegisterValue::undefined() {
        return Err(format!("global slot '{slot}' is undefined"));
    }
    Ok(value)
}
