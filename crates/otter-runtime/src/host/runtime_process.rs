use std::path::PathBuf;
use std::sync::Arc;

use otter_vm::RuntimeState;
use otter_vm::payload::{VmTrace, VmValueTracer};

use super::IsolatedEnvStore;

const HOST_PROCESS_SLOT: &str = "__otter_host_process";

/// Process metadata exposed through the host integration layer.
#[derive(Debug, Clone)]
pub struct HostProcessConfig {
    pub argv: Vec<String>,
    pub exec_argv: Vec<String>,
    pub exec_path: String,
    pub cwd: PathBuf,
}

impl Default for HostProcessConfig {
    fn default() -> Self {
        let exec_path = std::env::current_exe()
            .ok()
            .map(|path| path.to_string_lossy().to_string())
            .unwrap_or_else(|| "otter".to_string());
        let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
        Self {
            argv: vec![exec_path.clone()],
            exec_argv: Vec::new(),
            exec_path,
            cwd,
        }
    }
}

#[derive(Debug, Clone)]
struct RuntimeProcessPayload {
    process: HostProcessConfig,
    env_store: Arc<IsolatedEnvStore>,
}

impl VmTrace for RuntimeProcessPayload {
    fn trace(&self, _tracer: &mut dyn VmValueTracer) {}
}

pub(crate) fn install_runtime_process(
    runtime: &mut RuntimeState,
    process: HostProcessConfig,
    env_store: Arc<IsolatedEnvStore>,
) {
    let payload = runtime.alloc_native_object(RuntimeProcessPayload { process, env_store });
    runtime.install_global_value(
        HOST_PROCESS_SLOT,
        otter_vm::value::RegisterValue::from_object_handle(payload.0),
    );
}

#[must_use]
pub fn current_process(
    runtime: &mut RuntimeState,
) -> Option<(HostProcessConfig, Arc<IsolatedEnvStore>)> {
    let property = runtime.intern_property_name(HOST_PROCESS_SLOT);
    let global = runtime.intrinsics().global_object();
    let value = runtime.own_property_value(global, property).ok()?;
    let payload = runtime
        .native_payload_from_value::<RuntimeProcessPayload>(&value)
        .ok()?;
    Some((payload.process.clone(), payload.env_store.clone()))
}
