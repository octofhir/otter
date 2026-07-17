//! `node:diagnostics_channel` hosted module — named pub/sub channels (JS shim).

use otter_runtime::CapabilitySet;
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("diagnostics_channel.js");

/// CommonJS export: the `diagnostics_channel` namespace.
pub fn diagnostics_channel_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
) -> Result<Local<'scope>, String> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:diagnostics_channel", SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_diagnostics_channel_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
