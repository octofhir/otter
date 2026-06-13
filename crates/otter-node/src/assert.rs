//! `node:assert` / `assert` hosted module.
//!
//! Assert is largely a JavaScript surface in Node (a real `AssertionError`
//! class, matcher validation, deep equality, `rejects`/`doesNotReject`), so it
//! ships as a JS shim ([`SHIM`]) run through
//! [`otter_runtime::run_builtin_cjs_shim`]. Deep equality and value rendering
//! are delegated to `util`, which is injected as a dependency.
//!
//! # Contents
//! - [`assert_cjs_value`] - build `util`, then run the shim with it; the export
//!   is the callable `assert` (= `assert.ok`) with the comparison/throws/rejects
//!   methods and `AssertionError`/`CallTracker` attached.

use otter_runtime::CapabilitySet;
use otter_vm::{NativeCtx, Value};

/// Embedded `assert` implementation.
const SHIM: &str = include_str!("assert.js");

/// CommonJS export: the callable `assert` namespace.
pub fn assert_cjs_value(ctx: &mut NativeCtx<'_>, caps: &CapabilitySet) -> Result<Value, String> {
    let util = crate::util::util_cjs_value(ctx, caps)?;
    otter_runtime::run_builtin_cjs_shim(ctx, "assert", SHIM, &[("util", util)])
}

/// ESM namespace install — CommonJS is the supported surface for now.
pub fn install_assert_module(_ctx: &mut otter_runtime::HostedModuleCtx<'_>) -> Result<(), String> {
    Ok(())
}
