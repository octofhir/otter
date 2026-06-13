//! `node:buffer` / `buffer` hosted module + the global `Buffer`.
//!
//! `Buffer` is a `Uint8Array` subclass, so it ships as a JS shim ([`SHIM`]).
//! Because `Buffer` is also a global and `instanceof` must agree between the
//! global and `require('buffer').Buffer`, the class is defined once and cached
//! on `globalThis.Buffer`; the shim reuses the cached class on later runs.
//!
//! # Contents
//! - [`buffer_cjs_value`] - run the shim; export is `{ Buffer, SlowBuffer, … }`.
//! - [`SHIM`] - the embedded implementation, also run once at startup (by the
//!   node globals installer) to populate `globalThis.Buffer`.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

/// Embedded `Buffer` implementation.
pub const SHIM: &str = include_str!("buffer.js");

/// CommonJS export: the `buffer` namespace.
pub fn buffer_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:buffer", SHIM, &[])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_buffer_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
