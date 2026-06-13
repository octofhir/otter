//! `node:util` / `util` hosted module.
//!
//! A practical subset of Node's `util`, implemented as a dependency-free JS
//! shim ([`SHIM`]) run through [`otter_runtime::run_builtin_cjs_shim`]. `inspect`
//! (the suite's single most-used helper) and `format` are the focus, alongside
//! `types`, `promisify`, `inherits`, `isDeepStrictEqual`, `deprecate`, and the
//! ANSI/style helpers. Replaces the earlier native stub.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

/// Embedded `util` implementation.
const SHIM: &str = include_str!("util.js");

/// CommonJS export: the `util` namespace.
pub fn util_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:util", SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_util_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
