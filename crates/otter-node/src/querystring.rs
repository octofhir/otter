//! `node:querystring` / `querystring` hosted module — classic query-string
//! parse/stringify, a faithful JS port of Node v24 `lib/querystring.js` (with
//! the `internal/querystring` encoder helpers inlined). Requires `buffer`.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("querystring.js");

/// CommonJS export: the `querystring` namespace.
pub fn querystring_cjs_value(
    ctx: &mut NativeCtx<'_>,
    caps: &CapabilitySet,
) -> Result<Value, String> {
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:querystring", SHIM, &[("buffer", buffer)])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_querystring_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
