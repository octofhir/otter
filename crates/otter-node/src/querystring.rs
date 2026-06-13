//! `node:querystring` / `querystring` hosted module — classic query-string
//! parse/stringify, implemented as a dependency-free JS shim.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("querystring.js");

/// CommonJS export: the `querystring` namespace.
pub fn querystring_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:querystring", SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_querystring_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
