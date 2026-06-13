//! `node:readline` / `readline` + `node:readline/promises` hosted module —
//! line-oriented Interface over input/output streams (JS shim over `events`).

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("readline.js");

/// CommonJS export: the `readline` namespace.
pub fn readline_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let events = crate::events::events_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:readline", SHIM, &[("events", events)])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_readline_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
