//! Node-specific globals: `global` (alias for `globalThis`) and the
//! `setImmediate`/`clearImmediate` timer family.
//!
//! Web-platform globals (`atob`, `fetch`, `queueMicrotask`, `AbortController`,
//! ...) are NOT here — they live in `otter-web`.

use otter_runtime::{
    OtterError, Runtime, RuntimeGlobalInstaller, RuntimeNativeCtx as NativeCtx,
    RuntimeNativeError as NativeError, RuntimeValue as Value,
};

/// Installer for the Node-specific globals. Registered by `with_node_apis`.
#[must_use]
pub fn node_globals_installer() -> RuntimeGlobalInstaller {
    RuntimeGlobalInstaller::new(install)
}

fn install(runtime: &mut Runtime) -> Result<(), OtterError> {
    // `global` aliases `globalThis` (Node compatibility).
    let global_this = runtime.global_this();
    runtime.set_global("global", global_this);
    // TODO: real macrotask scheduling; present-but-minimal for now.
    runtime.install_native_global("setImmediate", 1, set_immediate)?;
    runtime.install_native_global("clearImmediate", 1, clear_immediate)?;
    Ok(())
}

fn set_immediate(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::number(otter_vm::number::NumberValue::from_i32(0)))
}

fn clear_immediate(_ctx: &mut NativeCtx<'_>, _args: &[Value]) -> Result<Value, NativeError> {
    Ok(Value::undefined())
}
