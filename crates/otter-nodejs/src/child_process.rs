use otter_macros::lodge;
use otter_runtime::{RegisterValue, RuntimeState, VmNativeCallError};

use crate::support::{install_method, install_readonly_value};

const CHILD_PROCESS_SLOT: &str = "__otter_node_child_process_export";

lodge!(
    child_process_module,
    module_specifiers = ["node:child_process", "child_process"],
    kind = commonjs,
    default = value(child_process_export_value(runtime)?),
);

fn child_process_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, CHILD_PROCESS_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    install_method(
        runtime,
        export,
        "spawnSync",
        3,
        child_process_spawn_sync,
        "child_process.spawnSync",
    )?;
    runtime.install_global_value(
        CHILD_PROCESS_SLOT,
        RegisterValue::from_object_handle(export.0),
    );
    Ok(RegisterValue::from_object_handle(export.0))
}

fn child_process_spawn_sync(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let result = runtime.alloc_object();
    let empty_stdout = RegisterValue::from_object_handle(runtime.alloc_string("").0);
    let empty_stderr = RegisterValue::from_object_handle(runtime.alloc_string("").0);
    install_readonly_value(runtime, result, "status", RegisterValue::from_i32(0))
        .map_err(|error| VmNativeCallError::Internal(error.into()))?;
    install_readonly_value(runtime, result, "signal", RegisterValue::null())
        .map_err(|error| VmNativeCallError::Internal(error.into()))?;
    install_readonly_value(runtime, result, "stdout", empty_stdout)
        .map_err(|error| VmNativeCallError::Internal(error.into()))?;
    install_readonly_value(runtime, result, "stderr", empty_stderr)
        .map_err(|error| VmNativeCallError::Internal(error.into()))?;
    Ok(RegisterValue::from_object_handle(result.0))
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
