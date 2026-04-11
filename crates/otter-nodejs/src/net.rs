use std::sync::{Arc, Mutex};

use otter_macros::lodge;
use otter_runtime::{ObjectHandle, RegisterValue, RuntimeState, VmNativeCallError};
use otter_vm::payload::{VmTrace, VmValueTracer};

use crate::support::{install_method, install_readonly_value, type_error};

const NET_EXPORT_SLOT: &str = "__otter_node_net_export";
const DEFAULT_AUTO_SELECT_TIMEOUT_MS: i32 = 250;

#[derive(Debug, Default)]
struct NetState {
    timeout_ms: i32,
}

#[derive(Debug, Clone)]
struct NetPayload {
    shared: Arc<Mutex<NetState>>,
}

impl VmTrace for NetPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

lodge!(
    net_module,
    module_specifiers = ["node:net", "net"],
    kind = commonjs,
    default = value(net_export_value(runtime)?),
);

fn net_export_value(runtime: &mut RuntimeState) -> Result<RegisterValue, String> {
    if let Ok(value) = read_global_slot(runtime, NET_EXPORT_SLOT) {
        return Ok(value);
    }

    let export = runtime.alloc_object();
    let payload = runtime.alloc_native_object(NetPayload {
        shared: Arc::new(Mutex::new(NetState {
            timeout_ms: DEFAULT_AUTO_SELECT_TIMEOUT_MS,
        })),
    });

    install_method(
        runtime,
        export,
        "getDefaultAutoSelectFamilyAttemptTimeout",
        0,
        net_get_default_auto_select_family_attempt_timeout,
        "net.getDefaultAutoSelectFamilyAttemptTimeout",
    )?;
    install_method(
        runtime,
        export,
        "setDefaultAutoSelectFamilyAttemptTimeout",
        1,
        net_set_default_auto_select_family_attempt_timeout,
        "net.setDefaultAutoSelectFamilyAttemptTimeout",
    )?;
    install_method(
        runtime,
        export,
        "createServer",
        1,
        net_create_server,
        "net.createServer",
    )?;
    install_readonly_value(
        runtime,
        export,
        "__otterState",
        RegisterValue::from_object_handle(payload.0),
    )?;

    runtime.install_global_value(NET_EXPORT_SLOT, RegisterValue::from_object_handle(export.0));
    Ok(RegisterValue::from_object_handle(export.0))
}

fn net_get_default_auto_select_family_attempt_timeout(
    this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let state = net_state_from_this(this, runtime)?;
    Ok(RegisterValue::from_i32(
        state
            .lock()
            .map_err(|_| VmNativeCallError::Internal("net state mutex poisoned".into()))?
            .timeout_ms,
    ))
}

fn net_set_default_auto_select_family_attempt_timeout(
    this: &RegisterValue,
    args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    let timeout = args
        .first()
        .and_then(|value| value.as_number())
        .unwrap_or(0.0)
        .max(0.0) as i32;
    let state = net_state_from_this(this, runtime)?;
    state
        .lock()
        .map_err(|_| VmNativeCallError::Internal("net state mutex poisoned".into()))?
        .timeout_ms = timeout;
    Ok(RegisterValue::undefined())
}

fn net_create_server(
    _this: &RegisterValue,
    _args: &[RegisterValue],
    runtime: &mut RuntimeState,
) -> Result<RegisterValue, VmNativeCallError> {
    Err(type_error(
        runtime,
        "net.createServer is not implemented yet",
    ))
}

fn net_state_from_this(
    this: &RegisterValue,
    runtime: &mut RuntimeState,
) -> Result<Arc<Mutex<NetState>>, VmNativeCallError> {
    let handle = this
        .as_object_handle()
        .map(ObjectHandle)
        .ok_or_else(|| type_error(runtime, "net receiver must be an object"))?;
    let property = runtime.intern_property_name("__otterState");
    let value = runtime
        .own_property_value(handle, property)
        .map_err(|_| type_error(runtime, "net receiver is missing state"))?;
    let Ok(payload) = runtime.native_payload_from_value::<NetPayload>(&value) else {
        return Err(type_error(runtime, "net receiver has invalid state"));
    };
    Ok(payload.shared.clone())
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
