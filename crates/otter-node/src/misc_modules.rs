//! Small `node:` module shims grouped together: `perf_hooks`, `v8`, `module`.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const PERF_HOOKS_SHIM: &str = include_str!("perf_hooks.js");
const V8_SHIM: &str = include_str!("v8.js");
const MODULE_SHIM: &str = include_str!("module_builtin.js");
const CLUSTER_SHIM: &str = include_str!("cluster.js");
const INTERNAL_UTIL_SHIM: &str = include_str!("internal_util.js");

/// `node:cluster` — single-process stub (always primary, no workers).
pub fn cluster_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let events = crate::events::events_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:cluster", CLUSTER_SHIM, &[("events", events)])
}

/// `node:perf_hooks` — performance timeline subset.
pub fn perf_hooks_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:perf_hooks", PERF_HOOKS_SHIM, &[])
}

/// `node:v8` — heap statistics + serialize/deserialize subset.
pub fn v8_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let buffer = crate::buffer::buffer_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:v8", V8_SHIM, &[("buffer", buffer)])
}

/// `node:module` — builtin-module metadata + a minimal Module class.
pub fn module_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:module", MODULE_SHIM, &[])
}

/// `internal/util` — the `--expose-internals` subset (sleep,
/// emitExperimentalWarning, deprecate, kEmptyObject).
pub fn internal_util_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "internal/util", INTERNAL_UTIL_SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_noop(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
