//! Hosted-module aliases that expose an existing runtime global, such as
//! `node:process`.
//!
//! # Contents
//! - [`process_cjs_value`] answers `require('process')` with the global.
//!
//! # Invariants
//! - Aliases return the already-rooted global value; they do not clone state.
//!
//! # See also
//! - `nodelib` for the small `node:` modules implemented in JavaScript.

use otter_runtime::{
    CapabilitySet, RuntimeLocal as Local, RuntimeNativeError as NativeError,
    RuntimeNativeScope as NativeScope, RuntimeTaskSpawner,
};

/// `node:process` / `process` — the exact `globalThis.process` object.
///
/// # Errors
/// Returns a type error when the runtime has not installed `process`.
pub fn process_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
    _module: Local<'scope>,
    _require: Local<'scope>,
) -> Result<Local<'scope>, NativeError> {
    scope
        .global("process")
        .ok_or_else(|| crate::type_error("process", "process global is not installed"))
}
