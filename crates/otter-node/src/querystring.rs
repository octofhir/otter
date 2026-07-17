//! `node:querystring` / `querystring` hosted module — classic query-string
//! parse/stringify, a faithful JS port of Node v24 `lib/querystring.js` (with
//! the `internal/querystring` encoder helpers inlined). Requires `buffer`.

use otter_runtime::CapabilitySet;
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("querystring.js");

/// CommonJS export: the `querystring` namespace.
pub fn querystring_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    caps: &CapabilitySet,
) -> Result<Local<'scope>, String> {
    let buffer = crate::buffer::buffer_cjs_value(scope, caps)?;
    otter_runtime::run_builtin_cjs_shim(scope, "node:querystring", SHIM, &[("buffer", buffer)])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_querystring_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
