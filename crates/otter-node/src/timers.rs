//! `node:timers` + `node:timers/promises` hosted modules — thin JS shims over
//! the global timer functions.

use otter_runtime::CapabilitySet;
use otter_vm::{Local, NativeScope};

const TIMERS_SHIM: &str = include_str!("timers.js");
const TIMERS_PROMISES_SHIM: &str = include_str!("timers_promises.js");

/// CommonJS export: the `timers` namespace.
pub fn timers_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
) -> Result<Local<'scope>, String> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:timers", TIMERS_SHIM, &[])
}

/// CommonJS export: the `timers/promises` namespace.
pub fn timers_promises_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
) -> Result<Local<'scope>, String> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:timers/promises", TIMERS_PROMISES_SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_noop(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
