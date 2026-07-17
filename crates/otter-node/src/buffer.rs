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

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

/// Embedded `Buffer` implementation.
pub const SHIM: &str = include_str!("buffer.js");

/// CommonJS export: the `buffer` namespace.
pub fn buffer_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:buffer", SHIM, &[])
}
