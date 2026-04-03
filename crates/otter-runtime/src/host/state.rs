use otter_vm::RuntimeState;

use super::HostConfig;
use super::module_runtime::{ModuleRuntimeSession, install_module_runtime_session};

/// Mutable host-owned state for one [`crate::OtterRuntime`] instance.
///
/// This stays runtime-local so the same host integration model can later back
/// worker runtimes / isolates without hidden process-global registries.
#[derive(Debug, Default)]
pub struct HostState {
    module_runtime: Option<ModuleRuntimeSession>,
}

impl HostState {
    pub(crate) fn ensure_module_runtime(
        &mut self,
        runtime: &mut RuntimeState,
        host: &HostConfig,
    ) -> ModuleRuntimeSession {
        if let Some(session) = self.module_runtime {
            return session;
        }

        let session = install_module_runtime_session(
            runtime,
            host.loader().clone(),
            host.native_modules().clone(),
        );
        self.module_runtime = Some(session);
        session
    }
}
