//! `node:events` / `events` hosted module — the `EventEmitter` class.
//!
//! `EventEmitter` is naturally expressed in JavaScript (prototype methods,
//! per-instance listener storage, `once` wrappers), so it ships as an embedded,
//! dependency-free CommonJS shim ([`SHIM`]) run through
//! [`otter_runtime::run_builtin_cjs_shim`]. It is the keystone dependency of the
//! stream/net/http modules.
//!
//! # Contents
//! - [`events_cjs_value`] - run the shim; export is the `EventEmitter`
//!   constructor with the `once`/`on`/`getEventListeners`/… statics attached.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

/// Embedded `EventEmitter` implementation.
const SHIM: &str = include_str!("events.js");

/// CommonJS export: the `EventEmitter` constructor (also its own `.EventEmitter`
/// plus the module-level statics).
pub fn events_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:events", SHIM, &[])
}
