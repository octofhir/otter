//! `node:readline` / `readline` + `node:readline/promises` hosted module —
//! line-oriented Interface over input/output streams (JS shim over `events`).

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("readline.js");

/// CommonJS export: the `readline` namespace.
pub fn readline_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:readline", SHIM, module, require)
}
