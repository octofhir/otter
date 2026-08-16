//! Native half of `node:async_hooks`.
//!
//! # Contents
//! - [`async_hooks_cjs_value`] installs the module's JavaScript shim.
//! - `getContext` / `setContext` expose the interpreter's async context, which
//!   microtasks and timers capture at enqueue and restore around execution.
//!
//! # Invariants
//! - The context is an ordinary JavaScript value owned by the isolate, so it
//!   is traced like any other root and never shared between isolates.
//! - Setting the context is not scoped: the caller restores the previous value
//!   itself, which is what lets `AsyncLocalStorage.run` be a `try`/`finally`.
//!
//! # See also
//! - `async_hooks.js`

use otter_runtime::{
    RuntimeLocal, RuntimeNativeCtx, RuntimeNativeError, RuntimeNativeScope, RuntimeValue,
};

/// Build the CommonJS export of `node:async_hooks`.
///
/// # Errors
/// Returns a native error when the shim fails to allocate or evaluate.
pub fn async_hooks_cjs_value<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
    _capabilities: &otter_runtime::CapabilitySet,
    _runtime_task_spawner: Option<otter_runtime::RuntimeTaskSpawner>,
    module: RuntimeLocal<'scope>,
    require: RuntimeLocal<'scope>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    // The shim reads the native surface off the global once at load; the
    // property is not part of the module's public shape.
    let native = build_native(scope)?;
    let globals = scope.global_this();
    scope.set(globals, "__otterAsyncContextNative", native)?;

    otter_runtime::run_builtin_cjs_shim(
        scope,
        "node:async_hooks",
        include_str!("async_hooks.js"),
        module,
        require,
    )
}

fn build_native<'scope>(
    scope: &mut RuntimeNativeScope<'scope, '_>,
) -> Result<RuntimeLocal<'scope>, RuntimeNativeError> {
    let object = scope.object()?;
    let get = scope.native_method("getContext", 0, get_context)?;
    scope.set(object, "getContext", get)?;
    let set = scope.native_method("setContext", 1, set_context)?;
    scope.set(object, "setContext", set)?;
    Ok(object)
}

fn get_context(
    ctx: &mut RuntimeNativeCtx<'_>,
    _args: &[RuntimeValue],
) -> Result<RuntimeValue, RuntimeNativeError> {
    Ok(ctx.async_context())
}

fn set_context(
    ctx: &mut RuntimeNativeCtx<'_>,
    args: &[RuntimeValue],
) -> Result<RuntimeValue, RuntimeNativeError> {
    let value = args
        .first()
        .copied()
        .unwrap_or_else(RuntimeValue::undefined);
    ctx.set_async_context(value);
    Ok(RuntimeValue::undefined())
}
