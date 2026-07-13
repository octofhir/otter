//! `node:tty` / `tty` hosted module.
//!
//! Otter does not currently own terminal handles, but ecosystem code commonly
//! probes TTY shape before deciding whether to emit ANSI color. This module
//! provides deterministic non-TTY stream objects instead of failing module
//! resolution.
//!
//! # Contents
//! - [`tty_cjs_value`] evaluates the small JS compatibility shim.
//! - [`install_tty_module`] reserves the ESM namespace for future native I/O.
//!
//! # Invariants
//! - No host file descriptor is opened or inspected.
//! - Streams default to `isTTY = false`, making color output opt-in.
//! - The module grants no filesystem or subprocess capability.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("tty.js");

/// CommonJS TTY compatibility namespace.
pub fn tty_cjs_value(ctx: &mut NativeCtx<'_>, _caps: &CapabilitySet) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "node:tty", SHIM, &[])
}

/// ESM namespace install. CommonJS is the supported constructor surface.
pub fn install_tty_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
