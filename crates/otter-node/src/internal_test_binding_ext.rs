//! `internal/test/binding` hosted module for Node compatibility tests.
//!
//! # Contents
//! - [`internal_test_binding_cjs_value`] returns the CommonJS test-binding shim.
//! - [`install_internal_test_binding_module`] is the ESM installer placeholder.
//!
//! # Invariants
//! - This module is harness-only. It exposes selected Node test hooks but is
//!   not a public `node:*` API.
//! - Hooks are stored as JS-owned global data; no VM handles cross native
//!   thread or async boundaries.
//!
//! # See also
//! - `internal_test_binding.js`

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

const SHIM: &str = include_str!("internal_test_binding.js");

pub fn install_internal_test_binding_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}

pub fn internal_test_binding_cjs_value(
    ctx: &mut NativeCtx<'_>,
    _caps: &CapabilitySet,
) -> Result<Value, String> {
    otter_runtime::run_builtin_cjs_shim(ctx, "internal/test/binding", SHIM, &[])
}
