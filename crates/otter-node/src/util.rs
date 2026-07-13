//! `node:util` / `util` hosted module.
//!
//! A practical subset of Node's `util`, implemented as a dependency-free JS
//! shim ([`SHIM`]) run through [`otter_runtime::run_builtin_cjs_shim`]. `inspect`
//! (the suite's single most-used helper) and `format` are the focus, alongside
//! `types`, `promisify`, `inherits`, `isDeepStrictEqual`, `deprecate`, dotenv
//! parsing, USV-string normalization, and the ANSI/style helpers.

use otter_runtime::CapabilitySet;
use otter_vm::{JsString, NativeCtx, NativeError, Value, native_function};

/// Embedded `util` implementation.
const SHIM: &str = include_str!("util.js");
const UTIL_EXPORT_CACHE_KEY: &str = "otter-internal:node-util-export";

/// Native backing for `util.getCallSites`: capture the live JS call
/// stack as a JSON array of call-site records. `args[0]` is the number
/// of frames to skip from the top (the JS `getCallSites` wrapper passes
/// `1` to hide its own frame); `args[1]` is the requested frame count.
/// Returns a JSON string the shim `JSON.parse`s into plain objects.
fn capture_call_sites(
    ctx: &mut NativeCtx<'_>,
    args: &[Value],
    _captures: &[Value],
) -> Result<Value, NativeError> {
    let skip = args
        .first()
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as usize)
        .unwrap_or(0);
    let count = args
        .get(1)
        .and_then(|v| v.as_f64())
        .filter(|n| n.is_finite() && *n >= 0.0)
        .map(|n| n as usize)
        .unwrap_or(10);
    let (interp, context) = ctx.interp_mut_and_context();
    let context = context.ok_or_else(|| NativeError::TypeError {
        name: "util.getCallSites",
        reason: "missing execution context".to_string(),
    })?;
    let json = interp.capture_call_sites_json(&context, skip, count);
    let s = JsString::from_str(&json, ctx.heap_mut()).map_err(|err| NativeError::TypeError {
        name: "util.getCallSites",
        reason: err.to_string(),
    })?;
    Ok(Value::string(s))
}

/// CommonJS export: the `util` namespace.
pub fn util_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    if let Some(cached) = ctx
        .interp_mut()
        .host_module_env_cached(UTIL_EXPORT_CACHE_KEY)
    {
        return Ok(Value::object(cached));
    }
    let callsites =
        native_function::native_value(ctx.heap_mut(), "captureCallSites", capture_call_sites)
            .map_err(|err| err.to_string())?;
    let export = otter_runtime::run_builtin_cjs_shim(
        ctx,
        "node:util",
        SHIM,
        &[("__otter_callsites", callsites)],
    )?;
    let export_object = export
        .as_object()
        .ok_or_else(|| "node:util shim did not return an object".to_string())?;
    ctx.interp_mut()
        .cache_host_module_env(std::sync::Arc::from(UTIL_EXPORT_CACHE_KEY), export_object);
    Ok(export)
}

/// CommonJS `util/types` export. Node exposes the same object by identity as
/// `require('util').types`.
pub fn util_types_cjs_value(
    ctx: &mut NativeCtx<'_>,
    caps: &CapabilitySet,
) -> Result<Value, String> {
    let util = util_cjs_value(ctx, caps)?;
    let (interp, exec) = ctx.interp_mut_and_context();
    let exec = exec.ok_or_else(|| "missing execution context for util/types".to_string())?;
    interp
        .get_property(&exec, util, "types")
        .map_err(|err| err.to_string())
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_util_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
