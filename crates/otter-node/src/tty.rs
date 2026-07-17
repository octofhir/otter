//! `node:tty` / `tty` hosted module.
//!
//! Otter does not currently own terminal handles, but ecosystem code commonly
//! probes TTY shape before deciding whether to emit ANSI color. This module
//! provides deterministic non-TTY stream objects instead of failing module
//! resolution.
//!
//! # Contents
//! - [`tty_cjs_value`] evaluates the small JS compatibility shim.
//!
//! # Invariants
//! - No host file descriptor is opened or inspected.
//! - Streams default to `isTTY = false`, making color output opt-in.
//! - The module grants no filesystem or subprocess capability.

use otter_runtime::{CapabilitySet, RuntimeNativeError as NativeError, RuntimeTaskSpawner};
use otter_vm::{Local, NativeScope};

const SHIM: &str = include_str!("tty.js");

/// CommonJS TTY compatibility namespace.
pub fn tty_cjs_value<'scope>(
    scope: &mut NativeScope<'scope, '_>,
    _caps: &CapabilitySet,
    _runtime_task_spawner: Option<RuntimeTaskSpawner>,
) -> Result<Local<'scope>, NativeError> {
    otter_runtime::run_builtin_cjs_shim(scope, "node:tty", SHIM, &[])
}
