use otter_macros::lodge;
use otter_runtime::{RegisterValue, RuntimeState, VmNativeCallError};
use otter_vm::{Interpreter, source};

use crate::support::{install_method, type_error};

const VM_EXPORT_SLOT: &str = "__otter_node_vm_export";

lodge!(
    vm_module,
    module_specifiers = ["node:vm", "vm"],
    kind = commonjs,
    default = value(vm_export_value(runtime)?),
);

fn vm_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, VM_EXPORT_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    install_method(
        runtime,
        export,
        "createContext",
        1,
        vm_create_context,
        "vm.createContext",
    )?;
    install_method(
        runtime,
        export,
        "runInContext",
        2,
        vm_run_in_context,
        "vm.runInContext",
    )?;
    install_method(
        runtime,
        export,
        "runInNewContext",
        2,
        vm_run_in_context,
        "vm.runInNewContext",
    )?;
    runtime.install_global_value(VM_EXPORT_SLOT, RegisterValue::from_object_handle(export.0));
    Ok(RegisterValue::from_object_handle(export.0))
}

fn vm_create_context(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Ok(args
        .first()
        .copied()
        .filter(|value| value.as_object_handle().is_some())
        .unwrap_or_else(|| RegisterValue::from_object_handle(runtime.alloc_object().0)))
}

fn vm_run_in_context(
    _this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let source_text = args
        .first()
        .copied()
        .ok_or_else(|| type_error(runtime, "vm.runInContext requires source text"))?;
    let source_text = runtime.js_to_string_infallible(source_text).into_string();
    let module = source::compile_script(&source_text, "node:vm").map_err(|error| {
        VmNativeCallError::Internal(format!("vm compile failed: {error}").into())
    })?;
    let interpreter = Interpreter::for_runtime(runtime);
    interpreter
        .execute_module(&module, runtime)
        .map(|result| result.return_value())
        .map_err(|error| {
            VmNativeCallError::Internal(format!("vm execution failed: {error}").into())
        })
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
