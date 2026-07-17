//! `node:readline` / `readline` + `node:readline/promises` hosted module —
//! line-oriented Interface over input/output streams (JS shim over `events`).

use otter_runtime::CapabilitySet;
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("readline.js");

/// CommonJS export: the `readline` namespace.
pub fn readline_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
) -> Result<Local<'scope>, String> {
    let events = crate::events::events_cjs_value(scope, caps)?;
    otter_runtime::run_builtin_cjs_shim(scope, "node:readline", SHIM, &[("events", events)])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_readline_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
