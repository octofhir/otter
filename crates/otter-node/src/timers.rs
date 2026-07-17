//! `node:timers` + `node:timers/promises` hosted modules — thin JS shims over
//! the global timer functions.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const TIMERS_SHIM: &str = include_str!("timers.js");
const TIMERS_PROMISES_SHIM: &str = include_str!("timers_promises.js");

/// CommonJS export: the `timers` namespace.
pub fn timers_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:timers", TIMERS_SHIM, module, require)
}

/// CommonJS export: the `timers/promises` namespace.
pub fn timers_promises_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    module: Local<'scope>,
    require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:timers/promises",
        TIMERS_PROMISES_SHIM,
        module,
        require,
    )
}
