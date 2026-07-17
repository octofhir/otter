//! `node:readline` / `readline` + `node:readline/promises` hosted module —
//! line-oriented Interface over input/output streams (JS shim over `events`).

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("readline.js");

/// CommonJS export: the `readline` namespace.
pub fn readline_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
    runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    let events = crate::events::events_cjs_value(scope, caps, runtime_task_spawner)?;
    otter_runtime::run_builtin_cjs_shim(scope, "node:readline", SHIM, &[("events", events)])
}
