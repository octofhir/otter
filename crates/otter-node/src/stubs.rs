//! Minimal stub modules so the Node test harness (`test/common`) can load.
//!
//! These expose just enough surface for `require('../common')` and `tmpdir.js`
//! to evaluate at load time. Behaviour is intentionally partial — full
//! implementations land per-module as conformance needs them.

use otter_runtime::{
    HostedModuleCtx, RuntimeNativeCtx as NativeCtx, RuntimeNativeError as NativeError,
    RuntimeValue as Value,
};

use crate::type_error;

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
