//! `internal/errors` hosted module for Node compatibility internals.
//!
//! # Contents
//! - [`internal_errors_cjs_value`] returns the CommonJS error factory shim.
//! - [`install_internal_errors_module`] is the ESM installer placeholder.
//!
//! # Invariants
//! - The module is internal to Node compatibility surfaces and fixtures.
//! - Error constructors stamp stable `.code` fields used by Node-visible APIs.
//!
//! # See also
//! - `internal_errors.js`

use otter_runtime::CapabilitySet;
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("internal_errors.js");

pub fn install_internal_errors_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}

pub fn internal_errors_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
) -> Result<Local<'scope>, String> {
    otter_runtime::run_builtin_cjs_shim(scope, "internal/errors", SHIM, &[])
}
