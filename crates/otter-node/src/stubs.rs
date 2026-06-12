//! Minimal stub modules so the Node test harness (`test/common`) can load.
//!
//! These expose just enough surface for `require('../common')` and `tmpdir.js`
//! to evaluate at load time. Behaviour is intentionally partial — full
//! implementations land per-module as conformance needs them.

use otter_runtime::{
    HostedModuleCtx, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeValue as Value,
};

use crate::{string_value, type_error};

/// Native stub that throws when invoked (the symbol exists for destructuring /
/// feature checks, but the operation is not implemented yet).
fn not_implemented(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Err(type_error("stub", "not implemented"))
}

/// `node:net` — placeholder namespace. The harness reads the default
/// auto-select-family timeout at load, so those accessors are provided.
pub fn install_net(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    ctx.builtin_method(
        "getDefaultAutoSelectFamilyAttemptTimeout",
        0,
        net_default_timeout,
    )?;
    ctx.builtin_method("setDefaultAutoSelectFamilyAttemptTimeout", 1, net_noop)?;
    Ok(())
}

fn net_default_timeout(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number(otter_vm::number::NumberValue::from_i32(10)))
}

fn net_noop(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}

/// `node:worker_threads` — only `isMainThread` is consulted at harness load.
pub fn install_worker_threads(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    ctx.property("isMainThread", Value::boolean(true))?;
    ctx.builtin_method("Worker", 1, not_implemented)?;
    Ok(())
}

/// `node:buffer` — `atob`/`btoa` are destructured by the harness.
pub fn install_buffer(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    ctx.builtin_method("atob", 1, not_implemented)?;
    ctx.builtin_method("btoa", 1, not_implemented)?;
    Ok(())
}

/// `node:url` — `pathToFileURL` is destructured by `tmpdir.js`.
pub fn install_url(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    ctx.builtin_method("pathToFileURL", 1, not_implemented)?;
    Ok(())
}

/// `node:child_process` — stub functions; harness uses them lazily.
pub fn install_child_process(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    for name in ["spawnSync", "spawn", "exec", "execSync", "execFileSync"] {
        ctx.builtin_method(name, 1, not_implemented)?;
    }
    Ok(())
}

/// `node:util` — basic `inspect`/`format`; the rest are stubs for now.
pub fn install_util(ctx: &mut HostedModuleCtx<'_>) -> Result<(), String> {
    ctx.builtin_method("inspect", 1, util_inspect)?;
    ctx.builtin_method("format", 1, util_format)?;
    ctx.builtin_method("getCallSites", 0, util_get_call_sites)?;
    ctx.builtin_method("inherits", 2, util_inherits)?;
    ctx.builtin_method("deprecate", 2, util_deprecate)?;
    Ok(())
}

fn util_inspect(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let rendered = args
        .first()
        .map(|v| v.display_string(ctx.heap()))
        .unwrap_or_default();
    string_value(ctx, &rendered)
}

fn util_format(ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    let parts: Vec<String> = args.iter().map(|v| v.display_string(ctx.heap())).collect();
    string_value(ctx, &parts.join(" "))
}

fn util_get_call_sites(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    // Returns undefined for now; the harness only reads call sites on failure.
    Ok(Value::undefined())
}

fn util_inherits(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}

fn util_deprecate(_ctx: &mut NativeCtx<'_>, args: &[Value]) -> Result<Value, NativeError> {
    // Return the wrapped function unchanged.
    Ok(args.first().copied().unwrap_or_else(Value::undefined))
}
