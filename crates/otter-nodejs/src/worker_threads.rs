use otter_macros::lodge;
use otter_runtime::{RegisterValue, RuntimeState, VmNativeCallError};

use crate::support::{install_readonly_value, install_value, type_error};

const WORKER_THREADS_SLOT: &str = "__otter_node_worker_threads_export";

lodge!(
    worker_threads_module,
    module_specifiers = ["node:worker_threads", "worker_threads"],
    kind = commonjs,
    default = value(worker_threads_export_value(runtime)?),
);

fn worker_threads_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, WORKER_THREADS_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    install_readonly_value(
        runtime,
        export,
        "isMainThread",
        RegisterValue::from_bool(true),
    )?;
    install_readonly_value(runtime, export, "threadId", RegisterValue::from_i32(0))?;
    install_readonly_value(runtime, export, "parentPort", RegisterValue::null())?;
    install_readonly_value(runtime, export, "workerData", RegisterValue::undefined())?;

    let constructor = runtime.alloc_object();
    install_value(
        runtime,
        export,
        "Worker",
        RegisterValue::from_object_handle(constructor.0),
    )?;

    let descriptor =
        otter_vm::descriptors::NativeFunctionDescriptor::method("Worker", 1, worker_constructor);
    let id = runtime.register_native_function(descriptor);
    let function = runtime.alloc_host_function(id);
    let property = runtime.intern_property_name("Worker");
    runtime
        .objects_mut()
        .set_property(
            export,
            property,
            RegisterValue::from_object_handle(function.0),
        )
        .map_err(|error| format!("failed to install worker_threads.Worker: {error:?}"))?;

    runtime.install_global_value(
        WORKER_THREADS_SLOT,
        RegisterValue::from_object_handle(export.0),
    );
    Ok(RegisterValue::from_object_handle(export.0))
}

fn worker_constructor(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Err(type_error(
        runtime,
        "worker_threads.Worker is not implemented yet",
    ))
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
