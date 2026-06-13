//! `node:diagnostics_channel` hosted module — named pub/sub channels (JS shim).

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("diagnostics_channel.js");

/// CommonJS export: the `diagnostics_channel` namespace.
pub fn diagnostics_channel_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:diagnostics_channel", SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_diagnostics_channel_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
