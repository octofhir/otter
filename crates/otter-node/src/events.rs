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

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

/// Embedded `EventEmitter` implementation.
const SHIM: &str = include_str!("events.js");

/// CommonJS export: the `EventEmitter` constructor (also its own `.EventEmitter`
/// plus the module-level statics).
pub fn events_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:events", SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_events_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
