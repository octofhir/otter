use otter_vm::RuntimeState;
use otter_vm::payload::{VmTrace, VmValueTracer};

use super::Capabilities;

const HOST_CAPABILITIES_SLOT: &str = "__otter_host_capabilities";

#[derive(Debug, Clone)]
struct RuntimeCapabilitiesPayload {
    capabilities: Capabilities,
}

impl VmTrace for RuntimeCapabilitiesPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

pub(crate) fn install_runtime_capabilities(runtime: &mut RuntimeState, capabilities: Capabilities) {
    let payload = runtime.alloc_native_object(RuntimeCapabilitiesPayload { capabilities });
    runtime.install_global_value(
        HOST_CAPABILITIES_SLOT,
        otter_vm::value::RegisterValue::from_object_handle(payload.0),
    );
}

#[must_use]
pub fn current_capabilities(runtime: &mut RuntimeState) -> Capabilities {
    let property = runtime.intern_property_name(HOST_CAPABILITIES_SLOT);
    let global = runtime.intrinsics().global_object();
    let Some(value) = runtime.own_property_value(global, property).ok() else {
        return Capabilities::none();
    };
    runtime
        .native_payload_from_value::<RuntimeCapabilitiesPayload>(&value)
        .map(|payload| payload.capabilities.clone())
        .unwrap_or_else(|_| Capabilities::none())
}
