//! `node:diagnostics_channel` hosted module — named pub/sub channels (JS shim).

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("diagnostics_channel.js");

/// CommonJS export: the `diagnostics_channel` namespace.
pub fn diagnostics_channel_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:diagnostics_channel", SHIM, &[])
}
