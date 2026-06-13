//! `node:test` / `test` hosted module — a minimal test-runner shim.
//!
//! Node's own `test/parallel` files increasingly drive their assertions through
//! `node:test` (`const { test } = require('node:test')`). The runner itself is
//! naturally expressed in JavaScript, so it ships as an embedded CommonJS shim
//! ([`SHIM`]) executed through [`otter_runtime::run_builtin_cjs_shim`]. The shim
//! depends only on `assert`, which is resolved natively and injected.
//!
//! # Contents
//! - [`node_test_cjs_value`] - build `assert`, then run the shim with it.
//! - [`install_node_test_module`] - ESM namespace install (delegates to the CJS
//!   value's own keys would require an interpreter; ESM consumers are rare for
//!   the test runner, so the namespace is left empty for now).
//!
//! # Invariants
//! - A failing test sets `process.exitCode = 1`; the conformance harness reads
//!   the process exit code, so all-pass leaves it at 0.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

/// Embedded `node:test` runner implementation.
const SHIM: &str = include_str!("node_test.js");

/// CommonJS export: the `test` function with `it`/`describe`/`suite`/hooks.
pub fn node_test_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let assert_value = crate::assert::assert_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "node:test", SHIM, &[("assert", assert_value)])
}

/// ESM namespace install — no eager members; CommonJS is the supported surface.
pub fn install_node_test_module(
    _ctx: &mut otter_runtime::HostedModuleCtx<'_>,
) -> Result<(), String> {
    Ok(())
}
